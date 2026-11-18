//! Short spoken replies for voice commands.
//!
//! Every string is at most [`MAX_SPOKEN_WORDS`] words so it stays a quick
//! interjection, and reads names with separators as spaces ("mac studio").

use crate::AgentRef;
use crate::Candidate;
use crate::VoiceCommand;
use crate::VoiceContext;
use crate::speakable;
use serde::Deserialize;
use serde::Serialize;

/// Spoken replies are capped so they stay a short interjection.
pub const MAX_SPOKEN_WORDS: usize = 25;
/// At most this many names are read out of a list.
const MAX_LISTED: usize = 3;
const MAX_MESSAGE_WORDS: usize = 10;
const MAX_PREVIEW_WORDS: usize = 12;

/// What an agent is doing, as the caller derives it from the app-server
/// thread status (`ThreadStatus` and its active flags).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentActivity {
    NotLoaded,
    Idle,
    Working,
    WaitingOnApproval,
    WaitingOnUserInput,
    Error,
}

/// Cap text at `max_words`, marking a cut with an ellipsis.
pub fn limit_words(text: &str, max_words: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= max_words {
        return words.join(" ");
    }
    format!("{}…", words[..max_words].join(" "))
}

fn spoken(text: String) -> String {
    limit_words(&text, MAX_SPOKEN_WORDS)
}

fn count_word(count: usize) -> String {
    const WORDS: &[&str] = &[
        "No", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten",
    ];
    WORDS
        .get(count)
        .map_or_else(|| count.to_string(), |word| (*word).to_string())
}

fn plural(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        singular.to_string()
    } else {
        plural.to_string()
    }
}

/// "a, b, and two more" with at most [`MAX_LISTED`] names.
fn list_names(names: &[String]) -> String {
    if names.len() <= MAX_LISTED {
        return names.join(", ");
    }
    let more = names.len() - MAX_LISTED;
    format!(
        "{}, and {} more",
        names[..MAX_LISTED].join(", "),
        count_word(more).to_lowercase()
    )
}

/// "a or b", "a, b, or c".
fn choice_list(candidates: &[Candidate]) -> String {
    let labels: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.label.as_str())
        .collect();
    match labels.as_slice() {
        [] => String::new(),
        [only] => (*only).to_string(),
        [first, second] => format!("{first} or {second}"),
        [init @ .., last] => format!("{}, or {last}", init.join(", ")),
    }
}

fn end_sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.ends_with(['…', '.', '!', '?']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

fn model_label(slug: &str, context: &VoiceContext) -> String {
    context
        .known_models
        .iter()
        .find(|model| model.slug == slug)
        .map_or_else(|| speakable(slug), crate::ModelRef::spoken_label)
}

fn agents_running(context: &VoiceContext) -> String {
    let agents = &context.known_agents;
    if agents.is_empty() {
        return "No agents running.".to_string();
    }
    let labels: Vec<String> = agents.iter().map(AgentRef::spoken_label).collect();
    format!(
        "{} {} running: {}.",
        count_word(agents.len()),
        plural(agents.len(), "agent", "agents"),
        list_names(&labels)
    )
}

fn machines_known(context: &VoiceContext) -> String {
    let machines: Vec<String> = context
        .known_machines
        .iter()
        .map(String::as_str)
        .map(speakable)
        .collect();
    if machines.is_empty() {
        return "No machines found.".to_string();
    }
    format!(
        "{} {}: {}.",
        count_word(machines.len()),
        plural(machines.len(), "machine", "machines"),
        list_names(&machines)
    )
}

/// What to say right after parsing.
///
/// Lists are answered directly from the context; destructive commands get a
/// "Say confirm." question; status, model, and machine commands get a short
/// acknowledgement. Returns `None` for [`VoiceCommand::None`].
pub fn spoken_response(command: &VoiceCommand, context: &VoiceContext) -> Option<String> {
    let text = match command {
        VoiceCommand::None => return None,
        VoiceCommand::ListAgents => agents_running(context),
        VoiceCommand::ListMachines => machines_known(context),
        VoiceCommand::AgentStatus { agent } => format!("Checking {}.", agent.spoken_label()),
        VoiceCommand::SteerAgent { agent, message, .. } => format!(
            "Tell {}: {} Say confirm.",
            agent.spoken_label(),
            end_sentence(&limit_words(message, MAX_MESSAGE_WORDS))
        ),
        VoiceCommand::StopAgent { agent, .. } => {
            format!("Stop {}? Say confirm.", agent.spoken_label())
        }
        VoiceCommand::SwitchModel { slug } => {
            format!("Switching to {}.", model_label(slug, context))
        }
        VoiceCommand::AttachMachine { machine } => {
            format!("Connecting to {}.", speakable(machine))
        }
        VoiceCommand::Ambiguous {
            action,
            heard,
            candidates,
        } => {
            let noun = action.target_noun();
            if candidates.len() <= MAX_LISTED {
                format!("Which {noun}: {}?", choice_list(candidates))
            } else {
                capitalize(&format!(
                    "{heard} matches {} {noun}s. Which one?",
                    count_word(candidates.len()).to_lowercase()
                ))
            }
        }
        VoiceCommand::NotFound { action, heard } => {
            let noun = action.target_noun();
            let article = if noun == "agent" { "an" } else { "a" };
            format!("I don't see {article} {noun} called {heard}.")
        }
    };
    Some(spoken(text))
}

/// What to say once a confirmed or immediate command has been carried out.
pub fn completed_speech(command: &VoiceCommand, context: &VoiceContext) -> Option<String> {
    let text = match command {
        VoiceCommand::SteerAgent { agent, .. } => format!("Sent to {}.", agent.spoken_label()),
        VoiceCommand::StopAgent { agent, .. } => format!("Stopped {}.", agent.spoken_label()),
        VoiceCommand::SwitchModel { slug } => {
            format!("Now using {}.", model_label(slug, context))
        }
        VoiceCommand::AttachMachine { machine } => {
            format!("Connected to {}.", speakable(machine))
        }
        VoiceCommand::None
        | VoiceCommand::ListAgents
        | VoiceCommand::AgentStatus { .. }
        | VoiceCommand::ListMachines
        | VoiceCommand::Ambiguous { .. }
        | VoiceCommand::NotFound { .. } => return None,
    };
    Some(spoken(text))
}

/// Answer "what's <agent> doing" from the thread status and, optionally, a
/// preview of its latest message or turn.
pub fn agent_status_speech(
    agent: &AgentRef,
    activity: AgentActivity,
    latest: Option<&str>,
) -> String {
    let label = agent.spoken_label();
    let state = match activity {
        AgentActivity::NotLoaded => "is not loaded",
        AgentActivity::Idle => "is idle",
        AgentActivity::Working => "is working",
        AgentActivity::WaitingOnApproval => "is waiting for your approval",
        AgentActivity::WaitingOnUserInput => "is waiting for your input",
        AgentActivity::Error => "hit an error",
    };
    let latest = latest
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        .map(|line| limit_words(line, MAX_PREVIEW_WORDS));
    let text = match latest {
        Some(latest) => format!("{label} {state}. Latest: {}", end_sentence(&latest)),
        None => format!("{label} {state}."),
    };
    spoken(capitalize(&text))
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
#[path = "speech_tests.rs"]
mod tests;
