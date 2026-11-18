//! `codex remote`: discover machines on the tailnet and in `~/.ssh/config`,
//! and attach the TUI to Codex running on one of them over SSH.
//!
//! `codex remote attach HOST` resolves to the same `ssh://` address that
//! `codex --remote ssh://HOST` accepts and launches the TUI through the same
//! code path.

use std::io::BufRead;
use std::io::Write;

use anyhow::Context;
use clap::Args;
use clap::Subcommand;
use codex_app_server_client::REMOTE_CODEX_BIN_ENV_VAR;
use codex_app_server_client::SshTarget;
use codex_tailnet::Machine;
use codex_tailnet::SshConfigSource;
use codex_tailnet::TailscaleCli;
use serde::Serialize;

#[derive(Debug, Args)]
pub(crate) struct RemoteCommand {
    #[command(subcommand)]
    pub(crate) subcommand: RemoteSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RemoteSubcommand {
    /// List machines on your Tailscale tailnet and in ~/.ssh/config.
    List(RemoteListArgs),

    /// Attach the TUI to Codex on a machine over SSH (`codex --remote ssh://HOST`).
    Attach(RemoteAttachArgs),
}

#[derive(Debug, Args)]
pub(crate) struct RemoteListArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RemoteAttachArgs {
    /// SSH destination: a tailnet machine name, MagicDNS name, IP, ~/.ssh/config
    /// alias, `user@host`, or `ssh://user@host:port`. Omit to pick from a list.
    #[arg(value_name = "HOST")]
    host: Option<String>,

    /// Codex executable on the remote machine (default: `codex` on its PATH).
    #[arg(long = "remote-codex-bin", value_name = "PATH", env = REMOTE_CODEX_BIN_ENV_VAR)]
    remote_codex_bin: Option<String>,
}

#[derive(Serialize)]
struct ListOutput<'a> {
    machines: &'a [Machine],
    warnings: &'a [String],
}

pub(crate) async fn run_list(args: RemoteListArgs) -> anyhow::Result<()> {
    let discovery = discover().await?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ListOutput {
                machines: &discovery.machines,
                warnings: &discovery.warnings,
            })?
        );
        return Ok(());
    }
    for warning in &discovery.warnings {
        eprintln!("warning: {warning}");
    }
    print!("{}", format_machine_table(&discovery.machines));
    Ok(())
}

/// Returns the `--remote` address for `codex remote attach`.
pub(crate) async fn attach_address(args: RemoteAttachArgs) -> anyhow::Result<String> {
    let host = match args.host {
        Some(host) => host,
        None => {
            let discovery = discover().await?;
            for warning in &discovery.warnings {
                eprintln!("warning: {warning}");
            }
            let stdin = std::io::stdin();
            let machine = pick_machine(
                &discovery.machines,
                &mut stdin.lock(),
                &mut std::io::stderr(),
            )?;
            machine.ssh_destination.clone()
        }
    };
    ssh_address(&host, args.remote_codex_bin)
}

async fn discover() -> anyhow::Result<codex_tailnet::Discovery> {
    let ssh_config = SshConfigSource::from_home();
    codex_tailnet::discover(&TailscaleCli::from_env(), ssh_config.as_ref())
        .await
        .map_err(anyhow::Error::from)
}

/// Builds the canonical `ssh://` address for a destination typed by the user.
fn ssh_address(host: &str, remote_codex_bin: Option<String>) -> anyhow::Result<String> {
    let host = host.trim();
    let url = if host.starts_with("ssh://") {
        host.to_string()
    } else {
        format!("ssh://{host}")
    };
    let mut target = SshTarget::parse_url(&url).map_err(anyhow::Error::msg)?;
    if let Some(remote_codex_bin) = remote_codex_bin.filter(|bin| !bin.trim().is_empty()) {
        target.remote_codex_bin = Some(remote_codex_bin);
    }
    Ok(target.display_url())
}

/// Machines worth offering in the picker: not this one and not known offline.
fn attach_candidates(machines: &[Machine]) -> Vec<&Machine> {
    machines
        .iter()
        .filter(|machine| !machine.is_self && machine.online != Some(false))
        .collect()
}

fn pick_machine<'a>(
    machines: &'a [Machine],
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> anyhow::Result<&'a Machine> {
    let candidates = attach_candidates(machines);
    if candidates.is_empty() {
        anyhow::bail!(
            "no online machines found; run `codex remote list` to see every discovered machine"
        );
    }
    writeln!(output, "Machines:")?;
    for (index, machine) in candidates.iter().enumerate() {
        writeln!(output, "  {:>2}) {}", index + 1, describe(machine))?;
    }
    loop {
        write!(output, "Attach to [1-{}]: ", candidates.len())?;
        output.flush()?;
        let mut line = String::new();
        if input
            .read_line(&mut line)
            .context("failed to read selection")?
            == 0
        {
            anyhow::bail!("no machine selected");
        }
        let line = line.trim();
        if line.is_empty() || line.eq_ignore_ascii_case("q") {
            anyhow::bail!("no machine selected");
        }
        match line.parse::<usize>() {
            Ok(choice) if (1..=candidates.len()).contains(&choice) => {
                return Ok(candidates[choice - 1]);
            }
            _ => writeln!(
                output,
                "Enter a number between 1 and {}, or q to cancel.",
                candidates.len()
            )?,
        }
    }
}

fn describe(machine: &Machine) -> String {
    let mut details = Vec::new();
    if let Some(os) = &machine.os {
        details.push(os.clone());
    }
    if machine.ssh_enabled == Some(true) {
        details.push("tailscale ssh".to_string());
    }
    if machine.online.is_none() {
        details.push("ssh config".to_string());
    }
    let details = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join(", "))
    };
    format!("{} -> {}{details}", machine.name, machine.ssh_destination)
}

fn format_machine_table(machines: &[Machine]) -> String {
    if machines.is_empty() {
        return "No machines found.\n".to_string();
    }
    let rows = machines
        .iter()
        .map(|machine| {
            let status = match (machine.is_self, machine.online) {
                (true, _) => "this machine",
                (false, Some(true)) => "online",
                (false, Some(false)) => "offline",
                (false, None) => "unknown",
            };
            let ssh = match (machine.ssh_enabled, &machine.ssh_alias) {
                (Some(true), _) => "tailscale",
                (_, Some(_)) => "config",
                _ => "-",
            };
            [
                machine.name.clone(),
                status.to_string(),
                machine.os.clone().unwrap_or_else(|| "-".to_string()),
                ssh.to_string(),
                machine.ssh_destination.clone(),
            ]
        })
        .collect::<Vec<_>>();
    let header = ["NAME", "STATUS", "OS", "SSH", "DESTINATION"].map(str::to_string);
    let mut widths = header.clone().map(|cell| cell.len());
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut table = String::new();
    for row in std::iter::once(&header).chain(rows.iter()) {
        let line = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect::<Vec<_>>()
            .join("  ");
        table.push_str(line.trim_end());
        table.push('\n');
    }
    table.push_str("\nAttach with `codex remote attach <DESTINATION>`, or run `codex remote attach` to pick.\n");
    table
}

#[cfg(test)]
#[path = "remote_cmd_tests.rs"]
mod tests;
