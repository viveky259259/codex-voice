//! Spoken orchestration commands during a voice call: "what's running", "what's
//! build doing", "tell build to run the tests", "stop build", "switch to opus".
//!
//! The widget decides synchronously whether a final transcript is a command (or
//! the answer to one), so ordinary speech never waits on the app. It mirrors the
//! app's orchestrator: the parser context and the agent "it" refers to arrive
//! with every answer, and the pending confirmation or "which one?" question is
//! predicted here and corrected by the answer. Commands themselves run in the
//! app, which owns the app-server connection.
//!
//! Only the user's own transcript runs a command. The voice model's handoff of
//! the same words, or of a "confirm" while a command waits, is swallowed and
//! never executed, matching the approval rule in `voice_status`.

use super::super::ChatWidget;
use super::voice_approval::parse_voice_approval_intent;
use crate::app_event::AppEvent;
use crate::app_event::VoiceCommandResult;
use codex_orchestrator::OutcomeStatus;
use codex_orchestrator::is_cancel_phrase;
use codex_orchestrator::parse_with_last_agent;
use codex_voice_commands::AgentRef;
use codex_voice_commands::Candidate;
use codex_voice_commands::VoiceAction;
use codex_voice_commands::VoiceCommand;
use codex_voice_commands::VoiceContext;
use codex_voice_commands::is_voice_confirmation;
use codex_voice_commands::resolve_choice;
use std::collections::VecDeque;

/// Transcripts remembered so the voice model's later handoff of the same
/// words is recognized as an echo.
const MAX_CONSUMED_VOICE_COMMANDS: usize = 4;

/// Voice-command bookkeeping for one call; reset with the call.
#[derive(Debug, Default)]
pub(crate) struct VoiceCommandState {
    /// The orchestrator's parser context, as last reported by the app.
    pub(super) context: VoiceContext,
    /// The agent "it" refers to.
    pub(super) last_agent: Option<AgentRef>,
    /// A steer or stop waiting for "confirm", or a "which one?" question.
    pub(super) pending: Option<PendingVoiceCommand>,
    /// Counter of requests that change the orchestrator's pending state. Only
    /// the answer to the latest one may correct `pending` and `last_agent`.
    pub(super) last_request: u64,
    /// Normalized user transcripts that were kept from the model.
    pub(super) consumed: VecDeque<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingVoiceCommand {
    /// A steer or stop; only a spoken "confirm" runs it.
    Confirmation(VoiceCommand),
    /// An ambiguous target; the next utterance picks one.
    Choice {
        action: VoiceAction,
        candidates: Vec<Candidate>,
    },
}

/// Where a final user transcript goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VoiceCommandRoute {
    /// A command or the answer to one: keep it from the model and run it.
    Run,
    /// Ordinary speech for the model.
    PassThrough,
    /// Ordinary speech that also drops a waiting command or question.
    CancelThenPassThrough { confirmation_cancelled: bool },
}

fn normalized(text: &str) -> String {
    text.to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

impl VoiceCommandState {
    /// The command the orchestrator would parse from `transcript` right now.
    pub(crate) fn parse(&self, transcript: &str, approval_prompt_open: bool) -> VoiceCommand {
        parse_with_last_agent(
            transcript,
            &self.context,
            self.last_agent.as_ref(),
            approval_prompt_open,
        )
    }

    /// Decide where a final user transcript goes, mirroring
    /// `Orchestrator::handle_utterance`, and predict the pending state the
    /// orchestrator will be in after it.
    pub(crate) fn route(
        &mut self,
        transcript: &str,
        approval_prompt_open: bool,
    ) -> VoiceCommandRoute {
        match self.pending.take() {
            Some(PendingVoiceCommand::Confirmation(command)) => {
                if is_voice_confirmation(transcript) {
                    if approval_prompt_open {
                        // "confirm" belongs to the open approval prompt.
                        self.pending = Some(PendingVoiceCommand::Confirmation(command));
                        return VoiceCommandRoute::PassThrough;
                    }
                    return VoiceCommandRoute::Run;
                }
                if is_cancel_phrase(transcript) {
                    return VoiceCommandRoute::Run;
                }
                // Anything else cancels; a new command still runs.
                self.route_fresh(transcript, approval_prompt_open)
                    .unwrap_or(VoiceCommandRoute::CancelThenPassThrough {
                        confirmation_cancelled: true,
                    })
            }
            Some(PendingVoiceCommand::Choice { action, candidates }) => {
                if is_cancel_phrase(transcript) {
                    return VoiceCommandRoute::Run;
                }
                if let Some(command) =
                    resolve_choice(&action, &candidates, transcript, &self.context)
                {
                    self.predict(&command);
                    return VoiceCommandRoute::Run;
                }
                self.route_fresh(transcript, approval_prompt_open)
                    .unwrap_or(VoiceCommandRoute::CancelThenPassThrough {
                        confirmation_cancelled: false,
                    })
            }
            None => self
                .route_fresh(transcript, approval_prompt_open)
                .unwrap_or(VoiceCommandRoute::PassThrough),
        }
    }

    fn route_fresh(
        &mut self,
        transcript: &str,
        approval_prompt_open: bool,
    ) -> Option<VoiceCommandRoute> {
        let command = self.parse(transcript, approval_prompt_open);
        if !command.is_command() {
            return None;
        }
        self.predict(&command);
        Some(VoiceCommandRoute::Run)
    }

    /// The state the orchestrator enters after executing `command`.
    fn predict(&mut self, command: &VoiceCommand) {
        match command {
            VoiceCommand::AgentStatus { agent } => self.last_agent = Some(agent.clone()),
            VoiceCommand::SteerAgent { agent, .. } | VoiceCommand::StopAgent { agent, .. } => {
                self.last_agent = Some(agent.clone());
                if command.needs_confirmation() {
                    self.pending = Some(PendingVoiceCommand::Confirmation(command.clone()));
                }
            }
            VoiceCommand::Ambiguous {
                action, candidates, ..
            } => {
                self.pending = Some(PendingVoiceCommand::Choice {
                    action: action.clone(),
                    candidates: candidates.clone(),
                });
            }
            VoiceCommand::None
            | VoiceCommand::ListAgents
            | VoiceCommand::SwitchModel { .. }
            | VoiceCommand::ListMachines
            | VoiceCommand::AttachMachine { .. }
            | VoiceCommand::NotFound { .. } => {}
        }
    }

    /// Start a request that changes the orchestrator's pending state.
    pub(crate) fn next_request(&mut self) -> u64 {
        self.last_request = self.last_request.wrapping_add(1);
        self.last_request
    }

    /// Adopt the orchestrator's state from its answer.
    pub(crate) fn apply_result(&mut self, result: &VoiceCommandResult) {
        self.context = result.context.clone();
        if result.seq != self.last_request {
            // A newer request is in flight; its answer will correct the rest.
            return;
        }
        self.last_agent = result.last_agent.clone();
        let Some(outcome) = result.outcome.as_ref() else {
            return;
        };
        self.pending = match (outcome.status, &outcome.command) {
            (OutcomeStatus::AwaitingConfirmation, command) => {
                Some(PendingVoiceCommand::Confirmation(command.clone()))
            }
            (
                OutcomeStatus::AwaitingChoice,
                VoiceCommand::Ambiguous {
                    action, candidates, ..
                },
            ) => Some(PendingVoiceCommand::Choice {
                action: action.clone(),
                candidates: candidates.clone(),
            }),
            _ => None,
        };
    }

    fn remember_consumed(&mut self, transcript: &str) {
        if self.consumed.len() >= MAX_CONSUMED_VOICE_COMMANDS {
            self.consumed.pop_front();
        }
        self.consumed.push_back(normalized(transcript));
    }

    fn take_consumed(&mut self, text: &str) -> bool {
        let wanted = normalized(text);
        match self.consumed.iter().position(|seen| *seen == wanted) {
            Some(index) => {
                self.consumed.remove(index);
                true
            }
            None => false,
        }
    }

    /// Whether the voice model's handoff of `text` belongs to voice commands
    /// and must be swallowed instead of becoming a model turn. Never runs or
    /// answers anything.
    pub(crate) fn claims_handoff(&mut self, text: &str, approval_prompt_open: bool) -> bool {
        if self.take_consumed(text) {
            // The user's transcript of these words already ran.
            return true;
        }
        match &self.pending {
            Some(PendingVoiceCommand::Confirmation(_)) => {
                if is_voice_confirmation(text)
                    || is_cancel_phrase(text)
                    || parse_voice_approval_intent(text).is_some()
                {
                    return true;
                }
            }
            Some(PendingVoiceCommand::Choice { action, candidates }) => {
                if is_cancel_phrase(text)
                    || parse_voice_approval_intent(text).is_some()
                    || resolve_choice(action, candidates, text, &self.context).is_some()
                {
                    return true;
                }
            }
            None => {}
        }
        self.parse(text, approval_prompt_open).is_command()
    }
}

impl ChatWidget {
    fn voice_commands_enabled(&self) -> bool {
        self.config.realtime.voice_commands && self.voice_status_available()
    }

    /// Route a final user transcript to the voice-command orchestrator.
    ///
    /// Returns `true` when the transcript is a command (or the answer to one)
    /// and must not be treated as a request for the model.
    pub(super) fn handle_realtime_voice_command(&mut self, text: &str) -> bool {
        if !self.voice_commands_enabled() {
            return false;
        }
        let Some(thread_id) = self.realtime_conversation.thread_id else {
            return false;
        };
        let approval_prompt_open = self.bottom_pane.current_approval_request().is_some();
        let commands = &mut self.realtime_conversation.voice_controls.voice_commands;
        match commands.route(text, approval_prompt_open) {
            VoiceCommandRoute::Run => {
                let seq = commands.next_request();
                commands.remember_consumed(text);
                self.app_event_tx.send(AppEvent::VoiceCommand {
                    thread_id,
                    seq,
                    transcript: text.to_string(),
                    approval_prompt_open,
                });
                true
            }
            VoiceCommandRoute::PassThrough => false,
            VoiceCommandRoute::CancelThenPassThrough {
                confirmation_cancelled,
            } => {
                let seq = commands.next_request();
                self.app_event_tx
                    .send(AppEvent::VoiceCommandCancel { thread_id, seq });
                if confirmation_cancelled {
                    self.add_info_message(
                        "Voice command cancelled.".to_string(),
                        /*hint*/ None,
                    );
                    self.speak_realtime_status("Cancelled.");
                }
                false
            }
        }
    }

    /// Whether a handed-off utterance is a voice command, or answers a waiting
    /// one. It is swallowed (hidden, never a model request) and never run:
    /// only the user's own transcript runs commands.
    pub(super) fn swallow_realtime_voice_command_handoff(&mut self, text: &str) -> bool {
        if !self.config.realtime.voice_commands {
            return false;
        }
        let approval_prompt_open = self.bottom_pane.current_approval_request().is_some();
        self.realtime_conversation
            .voice_controls
            .voice_commands
            .claims_handoff(text, approval_prompt_open)
    }

    /// Ask the app for a fresh agent snapshot when voice becomes active.
    pub(super) fn request_voice_command_refresh(&mut self) {
        if !self.config.realtime.voice_commands {
            return;
        }
        let Some(thread_id) = self.realtime_conversation.thread_id else {
            return;
        };
        let seq = self
            .realtime_conversation
            .voice_controls
            .voice_commands
            .last_request;
        self.app_event_tx
            .send(AppEvent::VoiceCommandRefresh { thread_id, seq });
    }

    /// Adopt the orchestrator's answer: speak it and show its screen text.
    pub(crate) fn on_voice_command_result(&mut self, result: &VoiceCommandResult) {
        self.realtime_conversation
            .voice_controls
            .voice_commands
            .apply_result(result);
        let Some(outcome) = result.outcome.as_ref() else {
            return;
        };
        let spoken = result
            .spoken_override
            .clone()
            .unwrap_or_else(|| outcome.spoken.clone());
        let screen = if result.spoken_override.is_some() || outcome.screen.trim().is_empty() {
            spoken.clone()
        } else {
            outcome.screen.clone()
        };
        self.speak_voice_command_result(&spoken, &screen);
    }

    /// Show a voice command's screen text and speak its short reply.
    pub(crate) fn speak_voice_command_result(&mut self, spoken: &str, screen: &str) {
        if !screen.trim().is_empty() {
            self.add_info_message(screen.to_string(), /*hint*/ None);
        }
        if !spoken.trim().is_empty() {
            self.speak_realtime_status(spoken);
        }
    }
}

#[cfg(test)]
#[path = "voice_commands_tests.rs"]
mod tests;
