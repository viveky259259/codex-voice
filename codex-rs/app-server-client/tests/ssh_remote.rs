//! Exercise `ssh://` remotes with a fake `ssh` that runs the remote command
//! locally. The "remote" Codex is a stub whose `app-server proxy` relays stdio
//! to a test WebSocket server on a Unix socket, standing in for the daemon's
//! control socket. `codex-cli`'s `remote_ssh` test runs the real binary.
#![cfg(unix)]

use std::error::Error;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_client::SshTarget;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::RequestId;
use codex_uds::UnixListener;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const RELAY_PY: &str = r#"
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

fn write_executable(path: &Path, body: &str) -> std::io::Result<()> {
    std::fs::write(path, body)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Writes a fake `ssh` that logs its argv, skips options through `--` and the
/// host, and runs the remote command with `sh -c` the way sshd would.
fn write_fake_ssh(dir: &Path, log: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join("ssh");
    write_executable(
        &path,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
while [ $# -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; break; fi
  shift
done
shift
exec /bin/sh -c "$*"
"#,
            log = log.display()
        ),
    )?;
    Ok(path)
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn connect_args(target: SshTarget) -> RemoteAppServerConnectArgs {
    RemoteAppServerConnectArgs {
        required_human_approval: false,
        endpoint: RemoteAppServerEndpoint::Ssh { target },
        client_name: "ssh-remote-test".to_string(),
        client_version: "0.0.0".to_string(),
        experimental_api: false,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 8,
    }
}

#[tokio::test]
async fn ssh_endpoint_initializes_and_round_trips_requests() -> Result<(), Box<dyn Error>> {
    if !python3_available() {
        eprintln!("skipping: python3 is required for the stdio relay stub");
        return Ok(());
    }
    let dir = TempDir::new()?;
    let socket_path = dir.path().join("control.sock");
    let log = dir.path().join("ssh.log");
    let relay = dir.path().join("relay.py");
    std::fs::write(&relay, RELAY_PY)?;
    let remote_codex = dir.path().join("remote codex");
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
            socket = socket_path.display()
        ),
    )?;
    let fake_ssh = write_fake_ssh(dir.path(), &log)?;

    let mut listener = UnixListener::bind(&socket_path).await?;
    let server = async {
        let stream = listener.accept().await?;
        let mut websocket = accept_async(stream).await?;
        let mut methods = Vec::new();
        while let Some(message) = websocket.next().await {
            let Message::Text(text) = message? else {
                continue;
            };
            let message: Value = serde_json::from_str(&text)?;
            let method = message["method"].as_str().unwrap_or_default().to_string();
            methods.push(method.clone());
            let response = match method.as_str() {
                "initialize" => json!({
                    "id": message["id"],
                    "result": {"userAgent": "codex_cli_rs/9.8.7 (test)", "codexHome": "/remote/home"},
                }),
                "echo" => json!({"id": message["id"], "result": message["params"]}),
                _ => continue,
            };
            websocket.send(Message::Text(response.to_string().into())).await?;
            if method == "echo" {
                break;
            }
        }
        Ok::<Vec<String>, Box<dyn Error>>(methods)
    };

    let mut target = SshTarget::parse_url(&format!(
        "ssh://me@fakehost:2222?codex_bin={}",
        url_encode(&remote_codex.display().to_string())
    ))?;
    target.ssh_program = Some(fake_ssh);
    let client = async {
        let client = RemoteAppServerClient::connect(connect_args(target)).await?;
        assert_eq!(client.server_version(), Some("9.8.7"));
        assert_eq!(client.codex_home(), Some("/remote/home"));
        let echoed = client
            .request_handle()
            .request_json_rpc(JSONRPCRequest {
                id: RequestId::Integer(7),
                method: "echo".to_string(),
                params: Some(json!({"over": "ssh"})),
                trace: None,
            })
            .await?;
        assert_eq!(echoed, Ok(json!({"over": "ssh"})));
        client.shutdown().await?;
        Ok::<(), Box<dyn Error>>(())
    };
    let (methods, ()) = tokio::try_join!(server, client)?;
    assert_eq!(methods, vec!["initialize", "initialized", "echo"]);

    let log = std::fs::read_to_string(&log)?;
    let invocations = log.lines().collect::<Vec<_>>();
    assert_eq!(invocations.len(), 2, "{log}");
    let quoted_bin = format!("'{}'", remote_codex.display());
    assert!(
        invocations[0].ends_with(&format!(
            "-p 2222 -l me -- fakehost {quoted_bin} app-server daemon start"
        )),
        "{log}"
    );
    assert!(
        invocations[1].ends_with(&format!("-- fakehost {quoted_bin} app-server proxy")),
        "{log}"
    );
    for invocation in invocations {
        assert!(invocation.starts_with("-o BatchMode=yes"), "{invocation}");
        assert!(invocation.contains("-o RequestTTY=no"), "{invocation}");
    }
    Ok(())
}

#[tokio::test]
async fn missing_remote_codex_is_reported() -> Result<(), Box<dyn Error>> {
    let dir = TempDir::new()?;
    let fake_ssh = write_fake_ssh(dir.path(), &dir.path().join("ssh.log"))?;
    let mut target = SshTarget::parse_url("ssh://fakehost?codex_bin=/nonexistent/codex")?;
    target.ssh_program = Some(fake_ssh);
    let err = RemoteAppServerClient::connect(connect_args(target))
        .await
        .err()
        .ok_or("connect should fail")?;
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(
        err.to_string().contains("CODEX_REMOTE_BIN"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn ssh_failures_explain_batch_mode() -> Result<(), Box<dyn Error>> {
    let dir = TempDir::new()?;
    let fake_ssh = dir.path().join("ssh");
    write_executable(
        &fake_ssh,
        "#!/bin/sh\necho 'me@fakehost: Permission denied (publickey).' >&2\nexit 255\n",
    )?;
    let mut target = SshTarget::parse_url("ssh://fakehost")?;
    target.ssh_program = Some(fake_ssh);
    let err = RemoteAppServerClient::connect(connect_args(target))
        .await
        .err()
        .ok_or("connect should fail")?;
    let message = err.to_string();
    assert!(message.contains("Permission denied (publickey)"), "{message}");
    assert!(message.contains("BatchMode=yes"), "{message}");
    Ok(())
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}
