//! Discover machines a Codex client can attach to over SSH.
//!
//! Machines come from the local Tailscale CLI (`tailscale status --json`) and
//! from concrete host aliases in `~/.ssh/config`. [`discover`] merges both,
//! removes duplicates, and sorts online machines first.

mod ssh_config;
mod tailscale;

use std::io;

use serde::Serialize;

pub use ssh_config::SshConfigSource;
pub use ssh_config::SshHost;
pub use ssh_config::parse_ssh_config;
pub use tailscale::TAILSCALE_BIN_ENV_VAR;
pub use tailscale::TailnetSource;
pub use tailscale::TailscaleCli;
pub use tailscale::TailscaleStatus;
pub use tailscale::parse_status;

/// Where a machine was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MachineSource {
    Tailscale,
    SshConfig,
}

/// A machine that can be reached over SSH.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    /// Short display name: the MagicDNS machine name, an SSH alias, or the OS
    /// hostname.
    pub name: String,
    /// The OS hostname reported by Tailscale, or the SSH config `HostName`.
    pub hostname: String,
    /// The MagicDNS name without its trailing dot.
    pub dns_name: Option<String>,
    /// Tailscale IPv4 and IPv6 addresses.
    pub ips: Vec<String>,
    /// `None` when the source cannot tell (SSH config entries).
    pub online: Option<bool>,
    /// Operating system reported by Tailscale.
    pub os: Option<String>,
    /// This is the machine running discovery.
    pub is_self: bool,
    /// Whether the machine advertises Tailscale SSH host keys. `None` when the
    /// source cannot tell. A machine without Tailscale SSH may still run sshd.
    pub ssh_enabled: Option<bool>,
    /// A matching `~/.ssh/config` alias, which carries the user's SSH settings.
    pub ssh_alias: Option<String>,
    /// The destination `codex remote attach` passes to `ssh`.
    pub ssh_destination: String,
    pub source: MachineSource,
}

impl Machine {
    fn from_ssh_host(host: SshHost) -> Self {
        Self {
            name: host.alias.clone(),
            hostname: host.hostname.unwrap_or_else(|| host.alias.clone()),
            dns_name: None,
            ips: Vec::new(),
            online: None,
            os: None,
            is_self: false,
            ssh_enabled: None,
            ssh_alias: Some(host.alias.clone()),
            ssh_destination: host.alias,
            source: MachineSource::SshConfig,
        }
    }

    fn matches_name(&self, candidate: &str) -> bool {
        let candidate = candidate.to_ascii_lowercase();
        self.name.eq_ignore_ascii_case(&candidate)
            || self.hostname.eq_ignore_ascii_case(&candidate)
            || self
                .dns_name
                .as_deref()
                .is_some_and(|dns_name| dns_name.eq_ignore_ascii_case(&candidate))
            || self
                .ips
                .iter()
                .any(|ip| ip.eq_ignore_ascii_case(&candidate))
    }

    fn online_rank(&self) -> u8 {
        match self.online {
            Some(true) => 0,
            None => 1,
            Some(false) => 2,
        }
    }
}

/// Why discovery could not read the tailnet.
#[derive(Debug, thiserror::Error)]
pub enum TailnetError {
    #[error(
        "Tailscale is not installed (tried {tried}). Install it from https://tailscale.com/download or set {TAILSCALE_BIN_ENV_VAR} to the tailscale CLI."
    )]
    NotInstalled { tried: String },
    #[error("the Tailscale service is not running ({detail}). Start Tailscale and try again.")]
    DaemonUnavailable { detail: String },
    #[error(
        "Tailscale is not logged in (state: {state}). Run `tailscale login` or `tailscale up`."
    )]
    NotLoggedIn { state: String },
    #[error("Tailscale is not running (state: {state}). Run `tailscale up`.")]
    NotRunning { state: String },
    #[error("`tailscale status --json` failed ({status}): {stderr}")]
    CommandFailed { status: String, stderr: String },
    #[error("`tailscale status --json` did not finish within {seconds}s")]
    Timeout { seconds: u64 },
    #[error("failed to parse `tailscale status --json` output: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("failed to run tailscale: {0}")]
    Io(#[from] io::Error),
}

/// Machines found by [`discover`], plus non-fatal problems.
#[derive(Debug, Default)]
pub struct Discovery {
    pub machines: Vec<Machine>,
    /// Sources that failed while another source still produced machines.
    pub warnings: Vec<String>,
}

/// Discovers machines from the tailnet and the SSH config.
///
/// A failing source becomes a warning while the other source still produces
/// machines. When no machine is found, the Tailscale error is returned so the
/// user learns why (not installed, not logged in, ...).
pub async fn discover<T: TailnetSource>(
    tailnet: &T,
    ssh_config: Option<&SshConfigSource>,
) -> Result<Discovery, TailnetError> {
    let mut warnings = Vec::new();
    let (tailnet_machines, tailnet_error) = match tailnet.machines().await {
        Ok(machines) => (machines, None),
        Err(err) => (Vec::new(), Some(err)),
    };
    let ssh_hosts = match ssh_config.map(SshConfigSource::hosts).transpose() {
        Ok(hosts) => hosts.unwrap_or_default(),
        Err(err) => {
            warnings.push(format!("failed to read ~/.ssh/config: {err}"));
            Vec::new()
        }
    };
    let machines = merge_machines(tailnet_machines, ssh_hosts);
    match tailnet_error {
        Some(err) if machines.is_empty() => Err(err),
        Some(err) => {
            warnings.insert(0, err.to_string());
            Ok(Discovery { machines, warnings })
        }
        None => Ok(Discovery { machines, warnings }),
    }
}

/// Merges tailnet machines with SSH config aliases.
///
/// An alias whose name or `HostName` matches a tailnet machine (by MagicDNS
/// name, machine name, OS hostname, or IP) is attached to that machine so
/// `ssh` picks up the alias's user, port, and keys. Other aliases are listed
/// on their own. Online machines sort first, then unknown, then offline; this
/// machine sorts after its peers within its group.
pub fn merge_machines(tailnet: Vec<Machine>, ssh_hosts: Vec<SshHost>) -> Vec<Machine> {
    let mut machines: Vec<Machine> = Vec::with_capacity(tailnet.len() + ssh_hosts.len());
    for machine in tailnet {
        let duplicate = machines.iter().any(|existing| {
            (existing.dns_name.is_some() && existing.dns_name == machine.dns_name)
                || existing
                    .ips
                    .iter()
                    .any(|ip| machine.ips.iter().any(|other| other == ip))
        });
        if !duplicate {
            machines.push(machine);
        }
    }
    for host in ssh_hosts {
        let matched = machines.iter_mut().find(|machine| {
            machine.source == MachineSource::Tailscale
                && (machine.matches_name(&host.alias)
                    || host
                        .hostname
                        .as_deref()
                        .is_some_and(|hostname| machine.matches_name(hostname)))
        });
        match matched {
            Some(machine) => {
                if machine.ssh_alias.is_none() {
                    machine.ssh_destination = host.alias.clone();
                    machine.ssh_alias = Some(host.alias);
                }
            }
            None => machines.push(Machine::from_ssh_host(host)),
        }
    }
    machines.sort_by(|left, right| {
        (
            left.online_rank(),
            left.is_self,
            left.name.to_ascii_lowercase(),
        )
            .cmp(&(
                right.online_rank(),
                right.is_self,
                right.name.to_ascii_lowercase(),
            ))
    });
    machines
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
