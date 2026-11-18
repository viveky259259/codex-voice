//! One-line activity summaries: short for speech, fuller for the screen.
//!
//! Speech never includes command output, diffs, reasoning, or more than the
//! first words of a message, and every spoken string is capped at
//! [`MAX_SPOKEN_WORDS`].

use crate::ActivityItem;
use crate::AgentInfo;
use crate::AgentReport;
use crate::AgentStatus;
use crate::ItemStatus;
use crate::MachineError;
use crate::MachineLink;
use crate::Snapshot;
use codex_voice_commands::AgentRef;
use codex_voice_commands::MAX_SPOKEN_WORDS;
use codex_voice_commands::limit_words;

/// Words of a command read aloud ("cargo test codex-core").
const COMMAND_WORDS: usize = 3;
/// Words of an agent message read aloud.
const MESSAGE_WORDS: usize = 10;
/// Characters per line on screen.
const SCREEN_LINE_CHARS: usize = 120;
/// Lines of command output shown on screen.
const SCREEN_OUTPUT_LINES: usize = 5;
/// Items of the newest turn shown on screen.
const SCREEN_ITEMS: usize = 8;
/// Line prefixes that mark a diff or code block; never read aloud.
const UNSPEAKABLE_PREFIXES: &[&str] = &["```", "diff --git", "@@", "+++", "---", "<<<<<<<"];

/// Cap a spoken reply at [`MAX_SPOKEN_WORDS`].
pub(crate) fn spoken(text: &str) -> String {
    limit_words(text, MAX_SPOKEN_WORDS)
}

pub(crate) fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn sentence(text: &str) -> String {
    let text = capitalize(text.trim_end());
    if text.ends_with(['.', '!', '?', '…']) {
        text
    } else {
        format!("{text}.")
    }
}

pub(crate) fn count_word(count: usize) -> String {
    const WORDS: &[&str] = &[
        "no", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    WORDS
        .get(count)
        .map_or_else(|| count.to_string(), |word| (*word).to_string())
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// The `codex-voice-commands` reference for an agent on a machine.
pub fn agent_ref(machine: &str, agent: &AgentInfo) -> AgentRef {
    AgentRef {
        id: agent.id.clone(),
        name: agent.name.clone(),
        machine: Some(machine.to_string()),
    }
}

/// A machine name as spoken: "mac-studio" is read "mac studio".
pub fn spoken_machine(machine: &str) -> String {
    AgentRef {
        id: String::new(),
        name: machine.to_string(),
        machine: None,
    }
    .spoken_label()
}

/// "cargo test codex-core" from `/bin/zsh -lc 'cargo test -p codex-core'`.
fn spoken_command(command: &str) -> String {
    let mut words: Vec<&str> = command.split_whitespace().collect();
    let is_shell = words
        .first()
        .map(|program| program.rsplit('/').next().unwrap_or(program))
        .is_some_and(|program| matches!(program, "bash" | "zsh" | "sh" | "fish"));
    if is_shell && words.get(1).is_some_and(|flag| flag.starts_with('-')) {
        words.drain(..2);
    }
    let spoken: Vec<&str> = words
        .into_iter()
        .map(|word| word.trim_matches(['\'', '"']))
        .filter(|word| !word.is_empty() && !word.starts_with('-'))
        .map(|word| word.rsplit('/').next().unwrap_or(word))
        .take(COMMAND_WORDS)
        .collect();
    if spoken.is_empty() {
        "a command".to_string()
    } else {
        spoken.join(" ")
    }
}

fn spoken_files(paths: &[String]) -> String {
    match paths {
        [only] => only.rsplit('/').next().unwrap_or(only).to_string(),
        paths => format!("{} files", count_word(paths.len())),
    }
}

/// The first speakable line of a message, without markdown, at most
/// [`MESSAGE_WORDS`] words. `None` for code, diffs, or empty text.
pub fn spoken_message(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    if UNSPEAKABLE_PREFIXES
        .iter()
        .any(|prefix| line.starts_with(prefix))
    {
        return None;
    }
    let cleaned: String = line
        .chars()
        .filter(|ch| !matches!(ch, '`' | '*' | '#' | '_' | '>'))
        .collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| limit_words(cleaned, MESSAGE_WORDS))
}

/// What an item says the agent is doing: "running cargo test", "editing
/// lib.rs", "calling search".
pub fn activity_phrase(item: &ActivityItem) -> String {
    match item {
        ActivityItem::UserMessage { .. } => "starting on a new message".to_string(),
        ActivityItem::AgentMessage { .. } => "writing a reply".to_string(),
        ActivityItem::Reasoning { .. } => "thinking".to_string(),
        ActivityItem::Plan { .. } => "planning".to_string(),
        ActivityItem::Command {
            command, status, ..
        } => {
            let command = spoken_command(command);
            match status {
                ItemStatus::InProgress => format!("running {command}"),
                ItemStatus::Completed => format!("ran {command}"),
                ItemStatus::Failed => format!("{command} failed"),
                ItemStatus::Declined => format!("{command} was declined"),
            }
        }
        ActivityItem::FileChange { paths, status } => {
            let files = spoken_files(paths);
            match status {
                ItemStatus::InProgress => format!("editing {files}"),
                ItemStatus::Completed => format!("edited {files}"),
                ItemStatus::Failed => format!("editing {files} failed"),
                ItemStatus::Declined => format!("edit to {files} was declined"),
            }
        }
        ActivityItem::ToolCall { tool, status } => match status {
            ItemStatus::InProgress => format!("calling {tool}"),
            ItemStatus::Completed => format!("called {tool}"),
            ItemStatus::Failed => format!("{tool} failed"),
            ItemStatus::Declined => format!("{tool} was declined"),
        },
        ActivityItem::Other { .. } => "working".to_string(),
    }
}

/// What approval is being waited on: "to run cargo test".
fn approval_object(item: Option<&ActivityItem>) -> Option<String> {
    match item? {
        ActivityItem::Command { command, .. } => {
            Some(format!("to run {}", spoken_command(command)))
        }
        ActivityItem::FileChange { paths, .. } => Some(format!("to edit {}", spoken_files(paths))),
        ActivityItem::ToolCall { tool, .. } => Some(format!("to call {tool}")),
        ActivityItem::UserMessage { .. }
        | ActivityItem::AgentMessage { .. }
        | ActivityItem::Reasoning { .. }
        | ActivityItem::Plan { .. }
        | ActivityItem::Other { .. } => None,
    }
}

/// "2 minutes", "just started", or "3 hours" since `started_at`.
pub fn elapsed_phrase(started_at: i64, now: i64) -> String {
    let seconds = now.saturating_sub(started_at).max(0);
    let (count, unit) = match seconds {
        0..60 => return "just started".to_string(),
        60..3600 => (seconds / 60, "minute"),
        _ => (seconds / 3600, "hour"),
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural}")
}

/// "build on desktop: running cargo test, 2 minutes" or "tests on laptop:
/// waiting for your approval to run cargo test".
///
/// `latest` overrides `agent.latest` when a fresher item is known.
pub fn agent_line(
    machine: &str,
    agent: &AgentInfo,
    latest: Option<&ActivityItem>,
    now: i64,
) -> String {
    let label = agent_ref(machine, agent).spoken_label();
    let latest = latest.or(agent.latest.as_ref());
    let state = match agent.status {
        AgentStatus::Running => {
            let doing = latest.map_or_else(|| "working".to_string(), activity_phrase);
            match agent.turn_started_at {
                Some(started_at) => format!("{doing}, {}", elapsed_phrase(started_at, now)),
                None => doing,
            }
        }
        AgentStatus::WaitingForApproval => match approval_object(latest) {
            Some(object) => format!("waiting for your approval {object}"),
            None => "waiting for your approval".to_string(),
        },
        AgentStatus::WaitingForInput => "waiting for your input".to_string(),
        AgentStatus::Idle => "idle".to_string(),
        AgentStatus::NotLoaded => "not loaded".to_string(),
        AgentStatus::Error => "hit an error".to_string(),
    };
    format!("{label}: {state}")
}

/// Agents that wait on the user, or failed, are read before everything else.
fn needs_user(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::WaitingForApproval | AgentStatus::WaitingForInput | AgentStatus::Error
    )
}

/// Agents that need the user come first when read aloud.
fn speech_priority(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::WaitingForApproval => 0,
        AgentStatus::WaitingForInput => 1,
        AgentStatus::Error => 2,
        AgentStatus::Running => 3,
        AgentStatus::Idle => 4,
        AgentStatus::NotLoaded => 5,
    }
}

/// "a", "a and b", "a, b and c"; more than three: `None`.
fn spoken_names(machines: &[&str]) -> Option<String> {
    let names: Vec<String> = machines.iter().map(|name| spoken_machine(name)).collect();
    match names.as_slice() {
        [] => Some(String::new()),
        [only] => Some(only.clone()),
        [init @ .., last] if names.len() <= 3 => Some(format!("{} and {last}", init.join(", "))),
        _ => None,
    }
}

/// One clause naming the machines that are not in the list: "Studio didn't
/// answer.", "Devbox is still connecting.", or "Devbox is still connecting,
/// studio didn't answer."
pub(crate) fn missing_machines_clause(connecting: &[&str], unreachable: &[&str]) -> Option<String> {
    let connecting_part = match connecting.len() {
        0 => None,
        1 => Some(format!("{} is still connecting", spoken_names(connecting)?)),
        count => Some(match spoken_names(connecting) {
            Some(names) => format!("{names} are still connecting"),
            None => format!("{} machines are still connecting", count_word(count)),
        }),
    };
    let unreachable_part = match unreachable.len() {
        0 => None,
        count => Some(match spoken_names(unreachable) {
            Some(names) => format!("{names} didn't answer"),
            None => format!("{} machines didn't answer", count_word(count)),
        }),
    };
    let clause = match (connecting_part, unreachable_part) {
        (None, None) => return None,
        (Some(part), None) | (None, Some(part)) => part,
        (Some(connecting), Some(unreachable)) => format!("{connecting}, {unreachable}"),
    };
    Some(sentence(&clause))
}

/// "Two agents. Tests on laptop: waiting for your approval. Build on
/// desktop: running cargo test, 2 minutes. Studio didn't answer."
///
/// Agents that need the user come first; the rest are grouped by machine in
/// backend order (this machine first). Machines that did not answer, or are
/// still connecting, are named in one closing clause.
pub(crate) fn list_speech(snapshot: &Snapshot, now: i64) -> String {
    let mut agents: Vec<(usize, &str, &AgentInfo)> = snapshot
        .machines
        .iter()
        .enumerate()
        .flat_map(|(index, machine)| {
            machine
                .agents
                .iter()
                .flatten()
                .map(move |agent| (index, machine.machine.as_str(), agent))
        })
        .collect();
    agents.sort_by_key(|(machine_index, _, agent)| {
        let priority = speech_priority(agent.status);
        if needs_user(agent.status) {
            (0, usize::from(priority), 0)
        } else {
            (1, *machine_index, priority)
        }
    });
    let agents: Vec<(&str, &AgentInfo)> = agents
        .into_iter()
        .map(|(_, machine, agent)| (machine, agent))
        .collect();
    let (connecting, unreachable): (Vec<_>, Vec<_>) = snapshot
        .failures()
        .partition(|(_, error)| matches!(error, MachineError::Connecting));
    let connecting: Vec<&str> = connecting.into_iter().map(|(machine, _)| machine).collect();
    let unreachable: Vec<&str> = unreachable
        .into_iter()
        .map(|(machine, _)| machine)
        .collect();
    let failure_note = missing_machines_clause(&connecting, &unreachable);
    let failures = [connecting, unreachable].concat();
    let header = match agents.len() {
        0 if failures.is_empty() => "No agents running.".to_string(),
        0 => "No agents found.".to_string(),
        1 => "One agent.".to_string(),
        count => format!("{} agents.", capitalize(&count_word(count))),
    };
    let mut parts = vec![header];
    let mut used = word_count(&parts[0]) + failure_note.as_deref().map_or(0, word_count);
    let mut spoken_agents = 0;
    for (index, (machine, agent)) in agents.iter().enumerate() {
        let line = sentence(&agent_line(machine, agent, /*latest*/ None, now));
        let left_after = agents.len() - index - 1;
        let tail = if left_after > 0 { 3 } else { 0 };
        if used + word_count(&line) + tail > MAX_SPOKEN_WORDS {
            break;
        }
        used += word_count(&line);
        parts.push(line);
        spoken_agents += 1;
    }
    let unspoken = agents.len() - spoken_agents;
    if unspoken > 0 && spoken_agents > 0 {
        parts.push(format!("Plus {} more.", count_word(unspoken)));
    }
    parts.extend(failure_note);
    spoken(&parts.join(" "))
}

/// "a, b, and two more" with at most three names, read as spoken.
fn listed_names(machines: &[&str]) -> String {
    let names: Vec<String> = machines.iter().map(|name| spoken_machine(name)).collect();
    if names.len() <= 3 {
        return names.join(", ");
    }
    format!(
        "{}, and {} more",
        names[..3].join(", "),
        count_word(names.len() - 3)
    )
}

/// "Three machines. Connected: desktop, devbox. Connecting: studio. Not
/// connected: laptop."
///
/// `machines` pairs each name with how the engine reaches it, or `None` for a
/// machine that is only known (discovered, not configured for orchestration).
pub(crate) fn machines_speech(machines: &[(String, Option<MachineLink>)]) -> String {
    if machines.is_empty() {
        return "No machines found.".to_string();
    }
    let with = |wanted: fn(Option<MachineLink>) -> bool| -> Vec<&str> {
        machines
            .iter()
            .filter(|(_, link)| wanted(*link))
            .map(|(name, _)| name.as_str())
            .collect()
    };
    let connected = with(|link| link.is_some_and(MachineLink::is_connected));
    let connecting = with(|link| link == Some(MachineLink::Connecting));
    let unreachable = with(|link| {
        matches!(
            link,
            Some(MachineLink::Unreachable | MachineLink::Disconnected)
        )
    });
    let others = with(|link| link.is_none());
    let mut parts = vec![match machines.len() {
        1 => "One machine.".to_string(),
        count => format!("{} machines.", capitalize(&count_word(count))),
    }];
    for (label, names) in [
        ("Connected", connected),
        ("Connecting", connecting),
        ("Unreachable", unreachable),
        ("Not connected", others),
    ] {
        if !names.is_empty() {
            parts.push(format!("{label}: {}.", listed_names(&names)));
        }
    }
    spoken(&parts.join(" "))
}

/// One line per machine with its orchestration state.
pub(crate) fn machines_screen(machines: &[(String, Option<MachineLink>)]) -> String {
    if machines.is_empty() {
        return "No machines found.".to_string();
    }
    machines
        .iter()
        .map(|(name, link)| {
            let state = match link {
                Some(MachineLink::Local) => "this machine, connected",
                Some(MachineLink::Connected) => "connected over SSH",
                Some(MachineLink::Connecting) => "connecting over SSH…",
                Some(MachineLink::Disconnected) => "not connected yet",
                Some(MachineLink::Unreachable) => "unreachable; retried on the next command",
                None => "not connected (add it to [realtime] voice_machines to orchestrate it)",
            };
            format!("{name}: {state}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn screen_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= SCREEN_LINE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(SCREEN_LINE_CHARS).collect();
    format!("{cut}…")
}

fn agent_screen_line(machine: &str, agent: &AgentInfo, now: i64) -> String {
    let line = agent_line(machine, agent, /*latest*/ None, now);
    match &agent.cwd {
        Some(cwd) => format!("{line}  [{}]", cwd.display()),
        None => line,
    }
}

/// Every agent on its own line, then one line per machine that failed.
pub(crate) fn list_screen(snapshot: &Snapshot, now: i64) -> String {
    let mut lines: Vec<String> = snapshot
        .agents()
        .map(|(machine, agent)| agent_screen_line(machine, agent, now))
        .collect();
    if lines.is_empty() {
        lines.push("No agents.".to_string());
    }
    lines.extend(snapshot.failures().map(|(machine, error)| match error {
        MachineError::Connecting => format!("{machine}: connecting…"),
        MachineError::Timeout { .. } | MachineError::Backend { .. } => {
            format!("{machine}: unreachable ({error})")
        }
    }));
    lines.join("\n")
}

/// "Build on desktop: running cargo test, 2 minutes. Last message: ..."
pub(crate) fn report_speech(machine: &str, report: &AgentReport, now: i64) -> String {
    let line = sentence(&agent_line(
        machine,
        &report.agent,
        report.recent.last(),
        now,
    ));
    let last_message = report.recent.iter().rev().find_map(|item| match item {
        ActivityItem::AgentMessage { text } => spoken_message(text),
        ActivityItem::UserMessage { .. }
        | ActivityItem::Reasoning { .. }
        | ActivityItem::Plan { .. }
        | ActivityItem::Command { .. }
        | ActivityItem::FileChange { .. }
        | ActivityItem::ToolCall { .. }
        | ActivityItem::Other { .. } => None,
    });
    match last_message {
        Some(message) if !report.agent.status.is_active() => {
            let with_message = format!("{line} Last message: {}", sentence(&message));
            if word_count(&with_message) <= MAX_SPOKEN_WORDS {
                with_message
            } else {
                spoken(&line)
            }
        }
        Some(_) | None => spoken(&line),
    }
}

fn item_screen_lines(item: &ActivityItem) -> Vec<String> {
    match item {
        ActivityItem::UserMessage { text } => vec![format!("> {}", screen_line(text))],
        ActivityItem::AgentMessage { text } => vec![format!("• {}", screen_line(text))],
        ActivityItem::Reasoning { summary } => vec![format!("… {}", screen_line(summary))],
        ActivityItem::Plan { text } => vec![format!("plan: {}", screen_line(text))],
        ActivityItem::Command {
            command,
            status,
            exit_code,
            output,
        } => {
            let exit = exit_code.map_or_else(String::new, |code| format!(", exit {code}"));
            let mut lines = vec![format!(
                "$ {} [{}{exit}]",
                screen_line(command),
                status_word(*status)
            )];
            if let Some(output) = output {
                let skip = output.lines().count().saturating_sub(SCREEN_OUTPUT_LINES);
                lines.extend(
                    output
                        .lines()
                        .skip(skip)
                        .map(|line| format!("  | {}", screen_line(line))),
                );
            }
            lines
        }
        ActivityItem::FileChange { paths, status } => vec![format!(
            "~ {} [{}]",
            screen_line(&paths.join(", ")),
            status_word(*status)
        )],
        ActivityItem::ToolCall { tool, status } => {
            vec![format!("tool {tool} [{}]", status_word(*status))]
        }
        ActivityItem::Other { kind } => vec![format!("({kind})")],
    }
}

fn status_word(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::InProgress => "running",
        ItemStatus::Completed => "done",
        ItemStatus::Failed => "failed",
        ItemStatus::Declined => "declined",
    }
}

/// The status line, the working directory, and the newest turn's items.
pub(crate) fn report_screen(machine: &str, report: &AgentReport, now: i64) -> String {
    let mut lines = vec![agent_line(
        machine,
        &report.agent,
        report.recent.last(),
        now,
    )];
    if let Some(cwd) = &report.agent.cwd {
        lines.push(format!("cwd: {}", cwd.display()));
    }
    let skip = report.recent.len().saturating_sub(SCREEN_ITEMS);
    lines.extend(report.recent.iter().skip(skip).flat_map(item_screen_lines));
    lines.join("\n")
}

#[cfg(test)]
#[path = "speech_tests.rs"]
mod tests;
