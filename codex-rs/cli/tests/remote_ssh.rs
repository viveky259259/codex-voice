//! End-to-end checks for `codex remote` and `ssh://` remotes.
//!
//! There is no tailnet or remote host in tests. A fake `ssh` first on `PATH`
//! drops its options and host and runs the remote command locally, so
//! `ssh://fakehost` reaches a real `codex app-server` through the real
//! `codex app-server daemon start` and `codex app-server proxy` commands.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;

const TAILSCALE_STATUS: &str = r#"{
  "BackendState": "Running",
  "Self": {"HostName": "laptop", "DNSName": "laptop.tail1234.ts.net.", "OS": "linux",
           "TailscaleIPs": ["100.64.0.1"], "Online": true},
  "CurrentTailnet": {"MagicDNSEnabled": true},
  "Peer": {
    "nodekey:a": {"HostName": "devbox", "DNSName": "devbox.tail1234.ts.net.", "OS": "linux",
                  "TailscaleIPs": ["100.64.0.2"], "Online": true,
                  "sshHostKeys": ["ssh-ed25519 AAAA"]},
    "nodekey:b": {"HostName": "old", "DNSName": "old.tail1234.ts.net.", "OS": "macOS",
                  "TailscaleIPs": ["100.64.0.3"], "Online": false}
  }
}"#;

fn write_executable(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn path_with(dir: &Path) -> Result<std::ffi::OsString> {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    Ok(std::env::join_paths(
        std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&inherited)),
    )?)
}

#[test]
fn remote_list_merges_tailscale_and_ssh_config() -> Result<()> {
    let home = TempDir::new()?;
    let bin = TempDir::new()?;
    std::fs::create_dir_all(home.path().join(".ssh"))?;
    std::fs::write(
        home.path().join(".ssh/config"),
        "Host work\n    HostName devbox.tail1234.ts.net\nHost pi\n    HostName 192.168.1.20\nHost *\n    User me\n",
    )?;
    let status = bin.path().join("status.json");
    std::fs::write(&status, TAILSCALE_STATUS)?;
    write_executable(
        &bin.path().join("tailscale"),
        &format!("#!/bin/sh\ncat '{}'\n", status.display()),
    )?;

    let output = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .env("HOME", home.path())
        .env("CODEX_HOME", home.path().join(".codex"))
        .env("PATH", path_with(bin.path())?)
        .env_remove("CODEX_TAILSCALE_BIN")
        .args(["remote", "list", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed: Value = serde_json::from_slice(&output.stdout)?;
    let summary = listed["machines"]
        .as_array()
        .context("machines array")?
        .iter()
        .map(|machine| {
            format!(
                "{} {} {} {}",
                machine["name"].as_str().unwrap_or_default(),
                machine["online"],
                machine["sshDestination"].as_str().unwrap_or_default(),
                machine["source"].as_str().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        vec![
            "devbox true work tailscale",
            "laptop true laptop.tail1234.ts.net tailscale",
            "pi null pi ssh-config",
            "old false old.tail1234.ts.net tailscale",
        ]
    );
    assert_eq!(listed["warnings"], serde_json::json!([]));
    Ok(())
}

#[test]
fn remote_list_explains_missing_tailscale() -> Result<()> {
    let home = TempDir::new()?;
    let empty_path = TempDir::new()?;
    let output = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .env("HOME", home.path())
        .env("CODEX_HOME", home.path().join(".codex"))
        .env("PATH", empty_path.path())
        // An explicit missing binary also skips the macOS Tailscale.app fallback.
        .env("CODEX_TAILSCALE_BIN", empty_path.path().join("tailscale"))
        .args(["remote", "list"])
        .output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Tailscale is not installed"), "{stderr}");
    Ok(())
}

#[tokio::test]
async fn exec_over_ssh_remote_reaches_a_real_app_server() -> Result<()> {
    use app_test_support::MockResponsesConfig;
    use app_test_support::TestAppServer;
    use app_test_support::create_final_assistant_message_sse_response;
    use core_test_support::responses;
    use tokio::process::Command;
    use tokio::time::Duration;
    use tokio::time::timeout;
    use wiremock::Mock;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    // macOS temporary paths can exceed the Unix socket path limit.
    #[cfg(target_os = "macos")]
    let server_home = tempfile::tempdir_in("/tmp")?;
    #[cfg(not(target_os = "macos"))]
    let server_home = TempDir::new()?;
    let codex = codex_utils_cargo_bin::cargo_bin("codex")?;
    let model = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(responses::sse_response(
            create_final_assistant_message_sse_response("hello over ssh")?,
        ))
        .mount(&model)
        .await;
    MockResponsesConfig::new(&model.uri())
        .with_root_config("features.plugins = false\nanalytics.enabled = false")
        .with_provider_config("env_key = \"CODEX_SSH_TEST_SERVER_API_KEY\"")
        .write(server_home.path())?;
    let _server = TestAppServer::builder()
        .with_program(&codex)
        .with_codex_home(server_home.path())
        // `codex app-server` does not accept the test-only startup-tasks flag.
        .with_plugin_startup_tasks()
        .without_managed_config()
        .with_args(&["app-server", "--listen", "unix://"])
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            ("CODEX_API_KEY", None),
            ("CODEX_ACCESS_TOKEN", None),
            (
                "CODEX_SSH_TEST_SERVER_API_KEY",
                Some("server-only-test-key"),
            ),
        ])
        .build()
        .await?;
    let socket_path = codex_app_server::app_server_control_socket_path(server_home.path())?;
    timeout(Duration::from_secs(30), async {
        while !socket_path.as_path().try_exists()? {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;

    // The fake remote: `ssh` logs its argv, sets the "remote" CODEX_HOME, and
    // runs the remote command with `sh -c`; `codex` there is the real binary.
    let bin = TempDir::new()?;
    let log = bin.path().join("ssh.log");
    write_executable(
        &bin.path().join("ssh"),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
while [ $# -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; break; fi
  shift
done
shift
export CODEX_HOME='{home}'
exec /bin/sh -c "$*"
"#,
            log = log.display(),
            home = server_home.path().display(),
        ),
    )?;
    std::os::unix::fs::symlink(&codex, bin.path().join("codex"))?;

    let client_home = TempDir::new()?;
    let output = timeout(
        Duration::from_secs(120),
        Command::new(&codex)
            .env("CODEX_HOME", client_home.path())
            .env("PATH", path_with(bin.path())?)
            .env_remove("CODEX_REMOTE_BIN")
            .env_remove("CODEX_SSH_TEST_SERVER_API_KEY")
            .current_dir(client_home.path())
            .kill_on_drop(true)
            .args([
                "exec",
                "--remote",
                "ssh://me@fakehost",
                "--json",
                "say hello",
            ])
            .output(),
    )
    .await??;
    let ssh_log = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        output.status.success(),
        "stderr: {}\nssh log:\n{ssh_log}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("hello over ssh"), "{stdout}");
    let events = stdout
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert!(events.iter().any(|event| event["type"] == "turn.completed"));

    let invocations = ssh_log.lines().collect::<Vec<_>>();
    assert_eq!(
        invocations
            .iter()
            .map(|line| line.rsplit_once(" -- ").map_or(*line, |(_, rest)| rest))
            .collect::<Vec<_>>(),
        vec![
            "fakehost codex app-server daemon start",
            "fakehost codex app-server proxy"
        ],
        "{ssh_log}"
    );
    for invocation in invocations {
        assert!(invocation.contains("-o BatchMode=yes"), "{invocation}");
        assert!(invocation.contains("-l me"), "{invocation}");
    }
    Ok(())
}
