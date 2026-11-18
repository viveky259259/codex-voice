//! Spoken orchestration, model, and machine commands for voice-first Codex.
//!
//! [`parse_voice_command`] maps one final voice transcript to a [`VoiceCommand`]
//! against a [`VoiceContext`] of the agents, machines, and models the caller
//! knows about. It is pure: no I/O, no clock, no async. The caller performs the
//! command (app-server requests, model switch, remote attach) and speaks the
//! strings from [`spoken_response`] and friends.
//!
//! Matching is deliberately conservative. An utterance only becomes a command
//! when it follows one of a few fixed shapes *and* its target resolves against
//! the context. Everything else is [`VoiceCommand::None`] and passes through as
//! an ordinary prompt, so "tell me how this function works" or "stop using
//! unwrap in parser.rs" never turn into orchestration.

mod parse;
mod resolve;
mod speech;
#[cfg(test)]
mod test_fixtures;
mod text;

pub use parse::is_voice_confirmation;
pub use parse::parse_voice_command;
pub use parse::resolve_choice;
pub use speech::AgentActivity;
pub use speech::MAX_SPOKEN_WORDS;
pub use speech::agent_status_speech;
pub use speech::completed_speech;
pub use speech::limit_words;
pub use speech::spoken_response;

use serde::Deserialize;
use serde::Serialize;

/// What the caller knows about when a transcript arrives.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceContext {
    /// Agents (app-server threads) that can be inspected, steered, or stopped.
    pub known_agents: Vec<AgentRef>,
    /// Machines that can be attached to, for example from `codex remote list`.
    pub known_machines: Vec<String>,
    /// Models that can be switched to.
    pub known_models: Vec<ModelRef>,
}

/// A running agent, typically one app-server thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRef {
    /// Opaque identifier, for example the app-server thread id.
    pub id: String,
    /// Display name, for example the thread name or agent nickname.
    pub name: String,
    /// The machine the agent runs on, when known.
    pub machine: Option<String>,
}

impl AgentRef {
    /// "build on desktop", or "build" when the machine is unknown.
    pub fn spoken_label(&self) -> String {
        let name = speakable(&self.name);
        match self.machine.as_deref() {
            Some(machine) => format!("{name} on {}", speakable(machine)),
            None => name,
        }
    }
}

/// A model that can be selected by voice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    /// The model slug passed to the model-switch path.
    pub slug: String,
    /// How people say it, for example `["opus", "claude opus"]`.
    pub spoken_aliases: Vec<String>,
}

impl ModelRef {
    /// The first alias, or the slug with separators read as spaces.
    pub fn spoken_label(&self) -> String {
        self.spoken_aliases
            .iter()
            .map(String::as_str)
            .find(|alias| !alias.trim().is_empty())
            .map_or_else(|| speakable(&self.slug), speakable)
    }
}

/// The result of parsing one final transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum VoiceCommand {
    /// Not a command: send the transcript to the model as a normal prompt.
    #[default]
    None,
    /// "what's running", "list agents".
    ListAgents,
    /// "what's build doing", "what is the desktop agent doing".
    AgentStatus { agent: AgentRef },
    /// "tell build to run the tests". Destructive: confirm first.
    #[serde(rename_all = "camelCase")]
    SteerAgent {
        agent: AgentRef,
        message: String,
        needs_confirmation: bool,
    },
    /// "stop build", "interrupt the laptop agent". Destructive: confirm first.
    #[serde(rename_all = "camelCase")]
    StopAgent {
        agent: AgentRef,
        needs_confirmation: bool,
    },
    /// "switch to claude", "use opus".
    SwitchModel { slug: String },
    /// "list my machines".
    ListMachines,
    /// "connect to my mac studio".
    AttachMachine { machine: String },
    /// The target matched more than one candidate. Ask which one, then call
    /// [`resolve_choice`] with the answer.
    Ambiguous {
        action: VoiceAction,
        /// The target as heard, normalized.
        heard: String,
        candidates: Vec<Candidate>,
    },
    /// The utterance named an agent, machine, or model explicitly ("the deploy
    /// agent") but nothing matched. Say so rather than prompting the model.
    NotFound { action: VoiceAction, heard: String },
}

impl VoiceCommand {
    /// Whether the command must be confirmed ("Say confirm.") before it runs.
    pub fn needs_confirmation(&self) -> bool {
        match self {
            Self::SteerAgent {
                needs_confirmation, ..
            }
            | Self::StopAgent {
                needs_confirmation, ..
            } => *needs_confirmation,
            Self::None
            | Self::ListAgents
            | Self::AgentStatus { .. }
            | Self::SwitchModel { .. }
            | Self::ListMachines
            | Self::AttachMachine { .. }
            | Self::Ambiguous { .. }
            | Self::NotFound { .. } => false,
        }
    }

    /// Whether the transcript should be consumed locally instead of being sent
    /// to the model.
    pub fn is_command(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// The command an [`VoiceCommand::Ambiguous`] or [`VoiceCommand::NotFound`]
/// result was going to run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum VoiceAction {
    AgentStatus,
    SteerAgent { message: String },
    StopAgent,
    SwitchModel,
    AttachMachine,
}

impl VoiceAction {
    pub(crate) fn target_noun(&self) -> &'static str {
        match self {
            Self::AgentStatus | Self::SteerAgent { .. } | Self::StopAgent => "agent",
            Self::SwitchModel => "model",
            Self::AttachMachine => "machine",
        }
    }
}

/// One possible target of an ambiguous command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    /// Agent id, model slug, or machine name.
    pub id: String,
    /// Speakable label, for example "build on desktop".
    pub label: String,
    /// For agent candidates, the machine the agent runs on. Thread ids are
    /// only unique per machine, so a choice is resolved by id and machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// Read separators and camelCase humps in names as spaces: "mac-studio" and
/// "MacStudio" become "mac studio" and "Mac Studio".
pub(crate) fn speakable(name: &str) -> String {
    let mut spaced = String::with_capacity(name.len() + 4);
    let mut previous: Option<char> = None;
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '/') {
            spaced.push(' ');
        } else {
            if previous.is_some_and(char::is_lowercase) && ch.is_uppercase() {
                spaced.push(' ');
            }
            spaced.push(ch);
        }
        previous = Some(ch);
    }
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}
