//! Configured machines and the SSH link's lifecycle, with a fake `ssh`.
//! `app/tests/voice_commands_tests.rs` drives a working link end to end.

use super::*;
use codex_orchestrator::AgentBackend;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::path::PathBuf;

/// A stdio relay to a Unix socket, standing in for `codex app-server proxy`
/// (copied from app-server-client's `tests/ssh_remote.rs`).
pub(crate) const RELAY_PY: &str = r#"
import os, socket, sys, threading
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(sys.argv[1])
def pump_stdin():
    while True:
        data = os.read(0, 65536)
        if not data:
            sock.shutdown(socket.SHUT_WR)
            return
        sock.sendall(data)
threading.Thread(target=pump_stdin, daemon=True).start()
while True:
    data = sock.recv(65536)
    if not data:
        break
    os.write(1, data)
"#;

pub(crate) fn write_executable(path: &Path, body: &str) -> std::io::Result<()> {
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

pub(crate) fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// A fake `ssh` that logs its argv and pid, skips options through `--` and
/// the host, and runs the remote command with `sh -c` the way sshd would.
pub(crate) fn write_fake_ssh(dir: &Path, log: &Path, pids: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join("ssh");
    write_executable(
        &path,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
printf '%s\n' "$$" >> '{pids}'
while [ $# -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; break; fi
  shift
done
shift
exec /bin/sh -c "$*"
"#,
            log = log.display(),
            pids = pids.display(),
        ),
    )?;
    Ok(path)
}

/// A remote `codex` whose `app-server proxy` relays to `socket`.
pub(crate) fn write_remote_codex(dir: &Path, socket: &Path) -> std::io::Result<PathBuf> {
    let relay = dir.join("relay.py");
    std::fs::write(&relay, RELAY_PY)?;
    let remote_codex = dir.join("remote codex");
    write_executable(
        &remote_codex,
        &format!(
            r#"#!/bin/sh
case "$*" in
  "app-server daemon start") echo '{{"status":"alreadyRunning"}}' ;;
  "app-server proxy") exec python3 '{relay}' '{socket}' ;;
  *) echo "unexpected remote command: $*" >&2; exit 2 ;;
esac
"#,
            relay = relay.display(),
            socket = socket.display()
        ),
    )?;
    Ok(remote_codex)
}

/// Whether a process is gone or a zombie (killed, not yet reaped).
#[cfg(target_os = "linux")]
pub(crate) fn process_ended(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_none_or(|state| matches!(state, "Z" | "X"))
}

fn machine(host: &str) -> VoiceMachine {
    VoiceMachine {
        host: host.to_string(),
        name: None,
        codex_bin: None,
    }
}

#[test]
fn configured_machines_resolve_to_ssh_targets_and_spoken_names() {
    let machines = vec![
        machine("devbox"),
        machine("me@mac-studio"),
        VoiceMachine {
            host: "ssh://me@10.0.0.7:2222".to_string(),
            name: Some("nas".to_string()),
            codex_bin: Some("/opt/codex/bin/codex".to_string()),
        },
        machine("pi.tail1234.ts.net"),
        machine("100.64.0.9"),
    ];
    let (resolved, warnings) = resolve_voice_machines(&machines, "desktop");
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(
        resolved
            .iter()
            .map(|machine| (machine.name.as_str(), machine.target.display_url()))
            .collect::<Vec<_>>(),
        vec![
            ("devbox", "ssh://devbox".to_string()),
            ("mac-studio", "ssh://me@mac-studio".to_string()),
            (
                "nas",
                "ssh://me@10.0.0.7:2222?codex_bin=%2Fopt%2Fcodex%2Fbin%2Fcodex".to_string()
            ),
            ("pi", "ssh://pi.tail1234.ts.net".to_string()),
            ("100.64.0.9", "ssh://100.64.0.9".to_string()),
        ]
    );
    assert_eq!(
        resolved[2].target.remote_codex_bin.as_deref(),
        Some("/opt/codex/bin/codex")
    );
}

#[test]
fn bad_duplicate_and_local_machines_are_skipped_with_a_warning() {
    let machines = vec![
        machine("devbox"),
        machine("ssh://devbox"),
        machine("-oProxyCommand=evil"),
        machine("ssh://me:secret@nas"),
        machine("desktop"),
        machine("devbox.example.com"),
        VoiceMachine {
            name: Some("devbox".to_string()),
            ..machine("other")
        },
    ];
    let (resolved, warnings) = resolve_voice_machines(&machines, "desktop");
    assert_eq!(
        resolved
            .iter()
            .map(|machine| machine.name.as_str())
            .collect::<Vec<_>>(),
        // The second "devbox" short name falls back to its full host.
        vec!["devbox", "devbox.example.com"]
    );
    assert_eq!(warnings.len(), 5, "{warnings:?}");
    assert!(warnings[0].contains("listed twice"), "{warnings:?}");
    assert!(warnings[1].contains("-oProxyCommand=evil"), "{warnings:?}");
    assert!(warnings[2].contains("passwords"), "{warnings:?}");
    assert!(warnings[3].contains("by this machine"), "{warnings:?}");
    assert!(warnings[4].contains("already used"), "{warnings:?}");
}

#[tokio::test]
async fn an_unreachable_machine_fails_each_call_and_retries_on_the_next() -> anyhow::Result<()> {
    let dir = tempfile::TempDir::new()?;
    let log = dir.path().join("ssh.log");
    let fake_ssh = dir.path().join("ssh");
    write_executable(
        &fake_ssh,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\necho 'ssh: connect to host nas port 22: No route to host' >&2\nexit 255\n",
            log.display()
        ),
    )?;
    let mut target = ssh_target("nas", None).map_err(anyhow::Error::msg)?;
    target.ssh_program = Some(fake_ssh);
    let link = RemoteLink::new(target).with_connect_wait(Duration::from_secs(10));
    assert_eq!(link.link(), MachineLink::Disconnected);
    let backend = AppServerAgentBackend::new("nas".to_string(), link);

    let Err(BackendError::Unreachable(error)) = backend.list_agents().await else {
        panic!("a dead machine is unreachable");
    };
    assert!(error.contains("ssh could not connect to `nas`"), "{error}");
    assert_eq!(backend.link(), MachineLink::Unreachable);

    // The next use connects again.
    assert!(matches!(
        backend.interrupt("t-1").await,
        Err(BackendError::Unreachable(_))
    ));
    let attempts = std::fs::read_to_string(&log)?;
    assert_eq!(attempts.lines().count(), 2, "{attempts}");
    assert!(
        attempts
            .lines()
            .all(|line| line.ends_with("-- nas codex app-server daemon start")),
        "{attempts}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn a_slow_machine_reports_connecting_without_blocking() -> anyhow::Result<()> {
    let dir = tempfile::TempDir::new()?;
    let fake_ssh = dir.path().join("ssh");
    write_executable(&fake_ssh, "#!/bin/sh\nexec sleep 30\n")?;
    let mut target = ssh_target("me@slow", Some("/opt/codex")).map_err(anyhow::Error::msg)?;
    target.ssh_program = Some(fake_ssh);
    let link = RemoteLink::new(target).with_connect_wait(Duration::from_millis(100));
    link.start_connecting();
    assert_eq!(link.link(), MachineLink::Connecting);
    let backend = AppServerAgentBackend::new("slow".to_string(), link);

    let started = std::time::Instant::now();
    assert_eq!(backend.list_agents().await, Err(BackendError::Connecting));
    assert_eq!(
        backend.steer("t-1", "run the tests").await,
        Err(BackendError::Connecting)
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    // Still the first attempt: no second ssh was started.
    assert_eq!(backend.link(), MachineLink::Connecting);
    Ok(())
}
