//! Read machines from `tailscale status --json`.
//!
//! The parsing rules follow T3 Code's `packages/tailscale` (MIT): DNS names
//! lose their trailing dot, `BackendState` decides whether the tailnet is
//! usable, and a missing field is treated as unknown rather than as an error.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::Machine;
use crate::MachineSource;
use crate::TailnetError;

/// Overrides the Tailscale CLI used for discovery.
pub const TAILSCALE_BIN_ENV_VAR: &str = "CODEX_TAILSCALE_BIN";

const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const MACOS_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
const MAX_STDERR_CHARS: usize = 400;

/// A source of tailnet machines.
pub trait TailnetSource {
    /// Returns every machine in the tailnet, including this one.
    fn machines(&self) -> impl Future<Output = Result<Vec<Machine>, TailnetError>> + Send;
}

/// Discovers machines by running the Tailscale CLI.
#[derive(Debug, Clone)]
pub struct TailscaleCli {
    candidates: Vec<OsString>,
}

impl TailscaleCli {
    /// Uses `CODEX_TAILSCALE_BIN` when set, otherwise `tailscale` on `PATH`
    /// and, on macOS, the CLI bundled inside Tailscale.app.
    pub fn from_env() -> Self {
        match std::env::var_os(TAILSCALE_BIN_ENV_VAR).filter(|value| !value.is_empty()) {
            Some(binary) => Self::with_binary(binary),
            None => Self::default(),
        }
    }

    /// Uses exactly this Tailscale CLI.
    pub fn with_binary(binary: impl Into<OsString>) -> Self {
        Self {
            candidates: vec![binary.into()],
        }
    }

    async fn status_json(&self) -> Result<String, TailnetError> {
        for candidate in &self.candidates {
            let mut command = Command::new(candidate);
            command
                .args(["status", "--json"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let output = match command.spawn() {
                Ok(child) => tokio::time::timeout(STATUS_TIMEOUT, child.wait_with_output())
                    .await
                    .map_err(|_| TailnetError::Timeout {
                        seconds: STATUS_TIMEOUT.as_secs(),
                    })??,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(TailnetError::Io(err)),
            };
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            if output.status.success() {
                return Ok(stdout);
            }
            // `tailscale status --json` exits non-zero while stopped on some
            // versions but still prints the status document.
            if stdout.trim_start().starts_with('{') {
                return Ok(stdout);
            }
            let stderr = sanitize_stderr(&String::from_utf8_lossy(&output.stderr));
            if is_daemon_unavailable(&stderr) {
                return Err(TailnetError::DaemonUnavailable { detail: stderr });
            }
            return Err(TailnetError::CommandFailed {
                status: output.status.to_string(),
                stderr,
            });
        }
        Err(TailnetError::NotInstalled {
            tried: self
                .candidates
                .iter()
                .map(|candidate| PathBuf::from(candidate).display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        })
    }
}

impl Default for TailscaleCli {
    fn default() -> Self {
        let mut candidates = vec![OsString::from("tailscale")];
        if cfg!(target_os = "macos") {
            candidates.push(OsString::from(MACOS_APP_CLI));
        }
        Self { candidates }
    }
}

impl TailnetSource for TailscaleCli {
    async fn machines(&self) -> Result<Vec<Machine>, TailnetError> {
        let raw = self.status_json().await?;
        parse_status(&raw)?.into_machines()
    }
}

/// A parsed `tailscale status --json` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleStatus {
    /// `BackendState`, e.g. `Running`, `Stopped`, or `NeedsLogin`.
    pub backend_state: Option<String>,
    /// `false` when the tailnet reports MagicDNS as disabled.
    pub magic_dns_enabled: bool,
    pub machines: Vec<Machine>,
}

impl TailscaleStatus {
    /// Returns the machines when the backend is running, or explains why the
    /// tailnet cannot be used.
    pub fn into_machines(self) -> Result<Vec<Machine>, TailnetError> {
        match self.backend_state.as_deref() {
            // Older CLIs omit BackendState; trust the peer list.
            Some("Running") | None => Ok(self.machines),
            Some(state @ ("NeedsLogin" | "NeedsMachineAuth")) => Err(TailnetError::NotLoggedIn {
                state: state.to_string(),
            }),
            Some(state) => Err(TailnetError::NotRunning {
                state: state.to_string(),
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawStatus {
    #[serde(rename = "BackendState")]
    backend_state: Option<String>,
    #[serde(rename = "Self")]
    self_peer: Option<RawPeer>,
    #[serde(rename = "Peer")]
    peers: Option<BTreeMap<String, RawPeer>>,
    #[serde(rename = "CurrentTailnet")]
    current_tailnet: Option<RawTailnet>,
}

#[derive(Debug, Deserialize)]
struct RawTailnet {
    #[serde(rename = "MagicDNSEnabled")]
    magic_dns_enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawPeer {
    #[serde(rename = "HostName")]
    host_name: Option<String>,
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
    #[serde(rename = "OS")]
    os: Option<String>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "Online")]
    online: Option<bool>,
    #[serde(rename = "sshHostKeys")]
    ssh_host_keys: Option<Vec<String>>,
}

/// Parses `tailscale status --json` output without judging `BackendState`.
pub fn parse_status(raw: &str) -> Result<TailscaleStatus, TailnetError> {
    let status: RawStatus = serde_json::from_str(raw)?;
    let running = status
        .backend_state
        .as_deref()
        .is_none_or(|state| state == "Running");
    let magic_dns_enabled = status
        .current_tailnet
        .as_ref()
        .and_then(|tailnet| tailnet.magic_dns_enabled)
        .unwrap_or(true);
    let mut machines = Vec::new();
    if let Some(self_peer) = status.self_peer {
        // `tailscale down` keeps Self from the last netmap, so Self.Online is
        // only meaningful together with BackendState.
        let online = running && self_peer.online.unwrap_or(true);
        if let Some(machine) = machine_from_peer(self_peer, true, online, magic_dns_enabled) {
            machines.push(machine);
        }
    }
    for peer in status.peers.unwrap_or_default().into_values() {
        let online = running && peer.online.unwrap_or(false);
        if let Some(machine) = machine_from_peer(peer, false, online, magic_dns_enabled) {
            machines.push(machine);
        }
    }
    Ok(TailscaleStatus {
        backend_state: status.backend_state,
        magic_dns_enabled,
        machines,
    })
}

fn machine_from_peer(
    peer: RawPeer,
    is_self: bool,
    online: bool,
    magic_dns_enabled: bool,
) -> Option<Machine> {
    let dns_name = peer
        .dns_name
        .as_deref()
        .map(normalize_dns_name)
        .filter(|name| !name.is_empty());
    let hostname = peer
        .host_name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    let ips = peer.tailscale_ips.unwrap_or_default();
    // The DNS label is the tailnet machine name and follows admin renames;
    // HostName is the OS hostname.
    let name = dns_name
        .as_deref()
        .and_then(|dns_name| dns_name.split('.').next())
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .or_else(|| hostname.clone())
        .or_else(|| ips.first().cloned())?;
    let ssh_destination = match (&dns_name, magic_dns_enabled) {
        (Some(dns_name), true) => dns_name.clone(),
        _ => ips
            .iter()
            .find(|ip| !ip.contains(':'))
            .or_else(|| ips.first())
            .cloned()
            .unwrap_or_else(|| name.clone()),
    };
    Some(Machine {
        hostname: hostname.unwrap_or_else(|| name.clone()),
        name,
        dns_name,
        ips,
        online: Some(online),
        os: peer.os.filter(|os| !os.is_empty()),
        is_self,
        ssh_enabled: Some(peer.ssh_host_keys.is_some_and(|keys| !keys.is_empty())),
        ssh_alias: None,
        ssh_destination,
        source: MachineSource::Tailscale,
    })
}

fn normalize_dns_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_string()
}

fn is_daemon_unavailable(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("failed to connect to local tailscale")
        || stderr.contains("is tailscale running")
        || stderr.contains("tailscaled")
}

/// Keeps error output short and drops auth keys that `tailscale` may echo.
fn sanitize_stderr(stderr: &str) -> String {
    let redacted = stderr
        .split_whitespace()
        .map(|word| {
            if word.contains("tskey-") {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if redacted.chars().count() <= MAX_STDERR_CHARS {
        redacted
    } else {
        let truncated: String = redacted.chars().take(MAX_STDERR_CHARS).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
#[path = "tailscale_tests.rs"]
mod tests;
