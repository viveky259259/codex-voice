//! What the engine returns: the [`Outcome`] of a command, the
//! [`PendingAction`] awaiting "confirm", and the replies for failures.

use crate::BackendError;
use crate::snapshot::CallError;
use crate::speech::capitalize;
use crate::speech::spoken;
use crate::speech::spoken_machine;
use codex_voice_commands::AgentRef;
use codex_voice_commands::VoiceCommand;
use serde::Deserialize;
use serde::Serialize;
/// Utterances that cancel a pending action or question outright.
const CANCEL_PHRASES: &[&str] = &[
    "cancel",
    "cancel it",
    "cancel that",
    "no",
    "nope",
    "no thanks",
    "never mind",
    "nevermind",
    "forget it",
    "stop",
    "don't",
    "dont",
];

/// What happened, what to say, and what to show.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub status: OutcomeStatus,
    /// Short reply to speak, at most `MAX_SPOKEN_WORDS` (25) words. Empty
    /// for [`OutcomeStatus::PassThrough`] unless a pending action was
    /// cancelled.
    pub spoken: String,
    /// Longer text for the screen: one line per agent, recent items, errors.
    pub screen: String,
    /// The command this outcome is about, after pronoun and choice
    /// resolution. For [`OutcomeStatus::ForCaller`] the caller performs it.
    pub command: VoiceCommand,
}

/// How an [`Outcome`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutcomeStatus {
    /// The command ran, or was answered from the snapshot.
    Done,
    /// A steer or stop waits for "confirm" ([`crate::Orchestrator::pending`]).
    AwaitingConfirmation,
    /// The target was ambiguous; the next utterance picks one.
    AwaitingChoice,
    /// A pending action was cancelled.
    Cancelled,
    /// The command could not run: unknown agent, machine down, RPC error.
    Failed,
    /// A model switch or machine attach: the caller performs `command`.
    ForCaller,
    /// Not a command. Send the transcript to the model unchanged.
    PassThrough,
}

/// A destructive command waiting for the user to say "confirm".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PendingAction {
    /// `turn/steer` (or `turn/start` when idle) with `message`.
    Steer { agent: AgentRef, message: String },
    /// `turn/interrupt`.
    Stop { agent: AgentRef },
}

impl PendingAction {
    pub fn agent(&self) -> &AgentRef {
        match self {
            Self::Steer { agent, .. } | Self::Stop { agent } => agent,
        }
    }

    /// The parsed command this action came from.
    pub fn command(&self) -> VoiceCommand {
        match self {
            Self::Steer { agent, message } => VoiceCommand::SteerAgent {
                agent: agent.clone(),
                message: message.clone(),
                needs_confirmation: true,
            },
            Self::Stop { agent } => VoiceCommand::StopAgent {
                agent: agent.clone(),
                needs_confirmation: true,
            },
        }
    }
}

fn normalized_phrase(text: &str) -> String {
    text.to_lowercase()
        .replace('\u{2019}', "'")
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '\''))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether an utterance cancels a pending action or question outright
/// ("cancel", "no", "never mind", "stop", ...).
pub fn is_cancel_phrase(text: &str) -> bool {
    CANCEL_PHRASES.contains(&normalized_phrase(text).as_str())
}

impl Outcome {
    pub(crate) fn new(
        status: OutcomeStatus,
        spoken_text: &str,
        screen: String,
        command: VoiceCommand,
    ) -> Self {
        Self {
            status,
            spoken: spoken(spoken_text),
            screen,
            command,
        }
    }

    pub(crate) fn pass_through() -> Self {
        Self {
            status: OutcomeStatus::PassThrough,
            spoken: String::new(),
            screen: String::new(),
            command: VoiceCommand::None,
        }
    }
}

pub(crate) fn cancelled(pending: &PendingAction) -> Outcome {
    let label = pending.agent().spoken_label();
    let screen = match pending {
        PendingAction::Steer { message, .. } => {
            format!("Cancelled: not sent to {label}: {message}")
        }
        PendingAction::Stop { .. } => format!("Cancelled: {label} keeps running."),
    };
    Outcome::new(
        OutcomeStatus::Cancelled,
        "Cancelled.",
        screen,
        pending.command(),
    )
}

pub(crate) fn already_idle(agent: &AgentRef, command: VoiceCommand) -> Outcome {
    let reply = format!(
        "{} is already idle. Nothing to stop.",
        capitalize(&agent.spoken_label())
    );
    Outcome::new(OutcomeStatus::Done, &reply, reply.clone(), command)
}

pub(crate) fn no_backend(agent: &AgentRef, command: VoiceCommand) -> Outcome {
    let machine = agent.machine.as_deref().unwrap_or("that machine");
    let reply = format!("I'm not connected to {}.", spoken_machine(machine));
    let screen = format!("No backend for machine `{machine}`.");
    Outcome::new(OutcomeStatus::Failed, &reply, screen, command)
}

pub(crate) fn failed(agent: &AgentRef, error: CallError, command: VoiceCommand) -> Outcome {
    let machine = agent.machine.as_deref().unwrap_or("that machine");
    let machine_name = spoken_machine(machine);
    let label = agent.spoken_label();
    let (reply, detail) = match &error {
        CallError::Timeout { after } => (
            format!("{} didn't answer.", capitalize(&machine_name)),
            format!("timed out after {}ms", after.as_millis()),
        ),
        CallError::Backend(BackendError::AgentNotFound(_)) => {
            (format!("I can't find {label} anymore."), error_text(&error))
        }
        CallError::Backend(BackendError::Unreachable(_)) => {
            (format!("I can't reach {machine_name}."), error_text(&error))
        }
        CallError::Backend(BackendError::Request(_)) => (
            format!("That failed on {machine_name}."),
            error_text(&error),
        ),
    };
    let screen = format!("{label}: {detail}");
    Outcome::new(OutcomeStatus::Failed, &reply, screen, command)
}

fn error_text(error: &CallError) -> String {
    match error {
        CallError::Timeout { after } => format!("timed out after {}ms", after.as_millis()),
        CallError::Backend(error) => error.to_string(),
    }
}
