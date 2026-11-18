//! Reach a remote app server through `ssh HOST -- codex app-server proxy`.
//!
//! `codex app-server proxy` relays its stdio to the remote daemon's control
//! socket, which speaks WebSocket. The client runs the same WebSocket
//! handshake it uses for a local Unix socket, but over the ssh child's
//! stdin/stdout. SSH authenticates the connection, so no bearer token is ever
//! sent over this transport.

use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::BufReader;
use tokio::io::ReadBuf;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_tungstenite::client_async_with_config;
use tracing::debug;
use tracing::warn;
use url::Url;

use crate::remote::UDS_WEBSOCKET_HANDSHAKE_URL;
use crate::remote::remote_websocket_config;
use crate::remote::websocket_request;
use crate::remote_websocket::WebSocketHandshake;

/// Overrides the Codex executable run on the remote host.
pub const REMOTE_CODEX_BIN_ENV_VAR: &str = "CODEX_REMOTE_BIN";
/// URL query parameter that overrides the remote Codex executable.
pub const REMOTE_CODEX_BIN_QUERY_PARAM: &str = "codex_bin";

const DEFAULT_REMOTE_CODEX_BIN: &str = "codex";
const DEFAULT_SSH_PROGRAM: &str = "ssh";
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(90);
// Covers the SSH handshake, remote process startup, and the WebSocket upgrade.
const SSH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STDERR_BYTES: usize = 4096;
// The remote shell reports a missing command with this status.
const COMMAND_NOT_FOUND_STATUS: i32 = 127;
// ssh reports its own failures (auth, host key, unreachable) with this status.
const SSH_ERROR_STATUS: i32 = 255;

/// An `ssh://[user@]host[:port][?codex_bin=PATH]` remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    /// Host name, SSH config alias, or IP address (IPv6 without brackets).
    pub host: String,
    pub port: Option<u16>,
    /// Codex executable on the remote host. Falls back to `CODEX_REMOTE_BIN`,
    /// then `codex` on the remote `PATH`.
    pub remote_codex_bin: Option<String>,
    /// Local ssh client. Defaults to `ssh` on `PATH`.
    pub ssh_program: Option<PathBuf>,
}

impl SshTarget {
    /// Parses `ssh://[user@]host[:port][?codex_bin=PATH]`.
    pub fn parse_url(addr: &str) -> Result<Self, String> {
        let invalid = |reason: &str| {
            format!(
                "invalid remote address `{addr}`: {reason}; expected `ssh://[user@]host[:port]`"
            )
        };
        let url = Url::parse(addr).map_err(|err| invalid(&err.to_string()))?;
        if url.scheme() != "ssh" {
            return Err(invalid("scheme must be `ssh`"));
        }
        if url.password().is_some() {
            return Err(invalid(
                "passwords are not supported; use SSH keys or an SSH agent",
            ));
        }
        if !matches!(url.path(), "" | "/") || url.fragment().is_some() {
            return Err(invalid("paths and fragments are not supported"));
        }
        let host = match url.host() {
            Some(url::Host::Domain(domain)) if !domain.is_empty() => domain.to_string(),
            Some(url::Host::Ipv4(addr)) => addr.to_string(),
            Some(url::Host::Ipv6(addr)) => addr.to_string(),
            Some(url::Host::Domain(_)) | None => return Err(invalid("missing host")),
        };
        // `ssh` would read a leading dash as an option.
        if host.starts_with('-') {
            return Err(invalid("host must not start with `-`"));
        }
        let user = match url.username() {
            "" => None,
            user => Some(
                percent_decode(user)
                    .filter(|user| !user.starts_with('-'))
                    .ok_or_else(|| invalid("invalid user name"))?,
            ),
        };
        let mut remote_codex_bin = None;
        for (key, value) in url.query_pairs() {
            if key != REMOTE_CODEX_BIN_QUERY_PARAM || value.is_empty() {
                return Err(invalid(&format!(
                    "unsupported query parameter `{key}`; only `{REMOTE_CODEX_BIN_QUERY_PARAM}` is accepted"
                )));
            }
            remote_codex_bin = Some(value.into_owned());
        }
        Ok(Self {
            user,
            host,
            port: url.port(),
            remote_codex_bin,
            ssh_program: None,
        })
    }

    /// The canonical `ssh://` address, accepted by `codex --remote`.
    pub fn display_url(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let mut address = String::from("ssh://");
        if let Some(user) = &self.user {
            address.push_str(&percent_encode_userinfo(user));
            address.push('@');
        }
        address.push_str(&host);
        if let Some(port) = self.port {
            address.push_str(&format!(":{port}"));
        }
        if let Some(bin) = &self.remote_codex_bin {
            let bin = url::form_urlencoded::byte_serialize(bin.as_bytes()).collect::<String>();
            address.push_str(&format!("?{REMOTE_CODEX_BIN_QUERY_PARAM}={bin}"));
        }
        address
    }

    fn resolved_remote_codex_bin(&self) -> String {
        self.remote_codex_bin
            .clone()
            .or_else(|| {
                std::env::var(REMOTE_CODEX_BIN_ENV_VAR)
                    .ok()
                    .filter(|bin| !bin.trim().is_empty())
            })
            .unwrap_or_else(|| DEFAULT_REMOTE_CODEX_BIN.to_string())
    }

    /// Arguments for `ssh` that run `remote_args` on the target with
    /// non-interactive, hardened options.
    pub fn ssh_args(&self, remote_codex_bin: &str, remote_args: &[&str]) -> Vec<String> {
        let mut args = Vec::new();
        for option in [
            // Never prompt: the TUI owns the terminal. Use keys, an agent, or
            // Tailscale SSH.
            "BatchMode=yes",
            "ConnectTimeout=10",
            // A host alias's RemoteCommand or RequestTTY must not replace ours.
            "RemoteCommand=none",
            "RequestTTY=no",
            // Forwardings from the user's config would collide across sessions.
            "ClearAllForwardings=yes",
            // Notice dead links so the TUI can reconnect.
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
        ] {
            args.push("-o".to_string());
            args.push(option.to_string());
        }
        if let Some(port) = self.port {
            args.push("-p".to_string());
            args.push(port.to_string());
        }
        if let Some(user) = &self.user {
            args.push("-l".to_string());
            args.push(user.clone());
        }
        args.push("--".to_string());
        args.push(self.host.clone());
        let remote_command = std::iter::once(remote_codex_bin)
            .chain(remote_args.iter().copied())
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        args.push(remote_command);
        args
    }

    fn command(&self, remote_codex_bin: &str, remote_args: &[&str]) -> Command {
        let program = self
            .ssh_program
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SSH_PROGRAM));
        let mut command = Command::new(program);
        command
            .args(self.ssh_args(remote_codex_bin, remote_args))
            .kill_on_drop(true);
        command
    }
}

fn percent_encode_userinfo(value: &str) -> String {
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

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Quotes one word for the remote POSIX shell that `sshd` runs the command in.
fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-./~:@%+=,".contains(&byte));
    if safe {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The ssh child's stdio, exposed as one duplex byte stream. Dropping it kills
/// the child.
pub(crate) struct SshStdio {
    _child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl AsyncRead for SshStdio {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for SshStdio {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<IoResult<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

pub(crate) async fn connect_ssh_endpoint(
    target: &SshTarget,
) -> IoResult<(String, WebSocketHandshake<SshStdio>)> {
    let endpoint = target.display_url();
    let remote_codex_bin = target.resolved_remote_codex_bin();
    let daemon_warning = ensure_remote_daemon(target, &endpoint, &remote_codex_bin).await?;

    let mut child = target
        .command(&remote_codex_bin, &["app-server", "proxy"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| spawn_error(target, err))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(IoError::other("ssh child is missing a piped stdio handle"));
    };
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let stderr_sink = Arc::clone(&stderr_tail);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            debug!(target: "codex_app_server_client::ssh", "ssh: {line}");
            if let Ok(mut tail) = stderr_sink.lock() {
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > MAX_STDERR_BYTES {
                    let mut cut = tail.len() - MAX_STDERR_BYTES;
                    while !tail.is_char_boundary(cut) {
                        cut += 1;
                    }
                    tail.drain(..cut);
                }
            }
        }
    });
    let stream = SshStdio {
        _child: child,
        stdin,
        stdout,
    };

    let request = websocket_request(UDS_WEBSOCKET_HANDSHAKE_URL).map_err(|err| {
        IoError::new(
            ErrorKind::InvalidInput,
            format!("invalid websocket handshake URL: {err}"),
        )
    })?;
    let handshake = timeout(
        SSH_HANDSHAKE_TIMEOUT,
        client_async_with_config(request, stream, Some(remote_websocket_config())),
    )
    .await;
    let failure = match handshake {
        Ok(Ok(handshake)) => return Ok((endpoint, handshake)),
        Ok(Err(err)) => format!("failed to connect to remote app server at `{endpoint}`: {err}"),
        Err(_) => format!("timed out connecting to remote app server at `{endpoint}`"),
    };
    // Give the stderr reader a moment to collect ssh's explanation.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stderr = stderr_tail
        .lock()
        .map(|tail| tail.trim().to_string())
        .unwrap_or_default();
    let mut message = failure;
    if !stderr.is_empty() {
        message.push_str(&format!("\nssh stderr:\n{stderr}"));
    }
    if let Some(daemon_warning) = daemon_warning {
        message.push_str(&format!("\n{daemon_warning}"));
    }
    Err(IoError::new(ErrorKind::ConnectionRefused, message))
}

/// Runs `codex app-server daemon start` on the target. `daemon start` is
/// idempotent and returns once the control socket answers `initialize`.
///
/// Returns a warning when the remote command ran but could not start the
/// daemon: an app server started some other way can still serve the proxy.
async fn ensure_remote_daemon(
    target: &SshTarget,
    endpoint: &str,
    remote_codex_bin: &str,
) -> IoResult<Option<String>> {
    let output = timeout(
        DAEMON_START_TIMEOUT,
        target
            .command(remote_codex_bin, &["app-server", "daemon", "start"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!(
                "timed out starting the app-server daemon on `{endpoint}` after {}s",
                DAEMON_START_TIMEOUT.as_secs()
            ),
        )
    })?
    .map_err(|err| spawn_error(target, err))?;
    if output.status.success() {
        debug!(
            stdout = %String::from_utf8_lossy(&output.stdout).trim(),
            "remote app-server daemon is running"
        );
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    match output.status.code() {
        Some(SSH_ERROR_STATUS) => Err(IoError::new(
            ErrorKind::ConnectionRefused,
            format!(
                "ssh could not connect to `{}`: {stderr}\nCodex runs ssh with BatchMode=yes, so the host must accept key, agent, or Tailscale SSH authentication without prompting, and its host key must already be trusted (run `ssh {}` once).",
                target.host, target.host
            ),
        )),
        Some(COMMAND_NOT_FOUND_STATUS) => Err(IoError::new(
            ErrorKind::NotFound,
            format!(
                "`{remote_codex_bin}` was not found on `{}`: {stderr}\nInstall Codex there, or point at it with {REMOTE_CODEX_BIN_ENV_VAR}=/path/to/codex or `?{REMOTE_CODEX_BIN_QUERY_PARAM}=/path/to/codex`.",
                target.host
            ),
        )),
        _ => {
            let warning = format!(
                "`{remote_codex_bin} app-server daemon start` failed on `{}` ({}): {stderr}",
                target.host, output.status
            );
            warn!("{warning}");
            Ok(Some(warning))
        }
    }
}

fn spawn_error(target: &SshTarget, err: IoError) -> IoError {
    let program = target.ssh_program.as_deref().map_or_else(
        || DEFAULT_SSH_PROGRAM.to_string(),
        |path| path.display().to_string(),
    );
    if err.kind() == ErrorKind::NotFound {
        IoError::new(
            ErrorKind::NotFound,
            format!(
                "ssh client `{program}` was not found; install OpenSSH to use `ssh://` remotes"
            ),
        )
    } else {
        IoError::new(err.kind(), format!("failed to run `{program}`: {err}"))
    }
}

#[cfg(test)]
#[path = "ssh_tests.rs"]
mod tests;
