//! Voice-first controls layered on the live call: auto-start, spoken approval
//! answers with a confirmation step, and short spoken status prompts.
//!
//! Approvals are applied through the visible approval prompt exactly as a key
//! press would apply them. An approve intent only arms a pending confirmation;
//! a following "confirm" for the same prompt applies it, and any other
//! utterance cancels it. Refusals apply immediately because they are safe.

use super::super::ChatWidget;
use super::NEXT_REALTIME_SPEECH_DELIVERY_ID;
use super::RealtimeAgentItemOrigin;
use super::RealtimeConversationPhase;
use super::RealtimeTurnOrigin;
use super::realtime_delegation_display_text;
use super::realtime_voice_supported;
use super::voice_approval::MAX_SPOKEN_WORDS;
use super::voice_approval::VoiceApprovalIntent;
use super::voice_approval::approval_confirmation_prompt;
use super::voice_approval::approval_option_confirmation_prompt;
use super::voice_approval::approval_option_phrase;
use super::voice_approval::approval_request_prompt;
use super::voice_approval::limit_words;
use super::voice_approval::parse_voice_approval_intent;
use super::voice_approval::parse_voice_approval_option;
use super::voice_approval::turn_error_speech;
use crate::app::app_server_requests::ResolvedAppServerRequest;
use crate::app_command::AppCommand;
use crate::bottom_pane::VoiceApprovalDecision;
use crate::bottom_pane::resolved_voice_key;
use codex_config::config_toml::RealtimeWsMode;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;

const MAX_STATUS_SPEECH_DELIVERIES: usize = 8;
const MAX_CONSUMED_VOICE_CONTROLS: usize = 4;

pub(crate) const VOICE_UNSUPPORTED_AUTO_START_NOTICE: &str = "Voice auto-start skipped: this build has no packaged voice host (codex-voice-host with GStreamer 1.28).";

/// Voice-first bookkeeping for one call; reset with the call.
#[derive(Default)]
pub(crate) struct VoiceControlState {
    /// An approve intent waiting for its spoken confirmation.
    pub(super) voice_approval: Option<PendingVoiceApproval>,
    /// Short spoken status prompts that have no text fallback to restore.
    pub(super) status_speech: VecDeque<u64>,
    /// Approval answers already handled by one path (the user's transcript or
    /// the model's handoff). The other path's copy of the same answer is
    /// swallowed instead of being applied a second time.
    pub(super) consumed_voice_controls: VecDeque<ConsumedVoiceControl>,
    /// Spoken orchestration commands: the mirrored parser context and any
    /// command waiting for "confirm" or "which one?".
    pub(super) voice_commands: super::voice_commands::VoiceCommandState,
}

/// Which copy of a spoken answer handled it. The realtime API delivers both the
/// user's input transcript and, sometimes, the model's handoff of the same
/// words, in either order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VoiceControlSource {
    Transcript,
    Handoff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConsumedVoiceControl {
    source: VoiceControlSource,
    intent: VoiceApprovalIntent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingVoiceApproval {
    request_key: String,
    intent: VoiceApprovalIntent,
    /// Set when a prompt option was chosen by number, letter or label.
    option: Option<usize>,
}

fn decision_for(intent: VoiceApprovalIntent) -> Option<VoiceApprovalDecision> {
    match intent {
        VoiceApprovalIntent::Approve => Some(VoiceApprovalDecision::ApproveOnce),
        VoiceApprovalIntent::ApproveForSession => Some(VoiceApprovalDecision::ApproveForSession),
        VoiceApprovalIntent::Deny => Some(VoiceApprovalDecision::Deny),
        VoiceApprovalIntent::Abort => Some(VoiceApprovalDecision::Abort),
        VoiceApprovalIntent::Confirm => None,
    }
}

impl ChatWidget {
    /// Start voice for a newly established thread when `[realtime] auto_start` is set.
    ///
    /// Returns `true` when a call attempt started. When this build cannot run voice,
    /// a single notice explains why instead of failing silently.
    pub(crate) fn auto_start_realtime_conversation(&mut self) -> bool {
        if !self.config.realtime.auto_start
            || self.realtime_conversation_is_running()
            || self.thread_id().is_none()
            || self.side_conversation_active()
            || self.blocks_direct_input
            || self.has_misalignment_policy_violation()
        {
            return false;
        }
        // Local voice needs neither the packaged voice host nor a realtime session type.
        if !self.local_voice_selected() && !realtime_voice_supported() {
            self.add_info_message(
                VOICE_UNSUPPORTED_AUTO_START_NOTICE.to_string(),
                /*hint*/ None,
            );
            return false;
        }
        if !self.local_voice_selected()
            && self.config.realtime.session_type != RealtimeWsMode::Conversational
        {
            self.add_info_message(
                "Voice auto-start skipped: set [realtime] type = \"conversational\".".to_string(),
                /*hint*/ None,
            );
            return false;
        }
        self.toggle_realtime_conversation();
        self.realtime_conversation_is_running()
    }

    #[cfg(test)]
    pub(crate) fn set_realtime_auto_start_for_test(&mut self, auto_start: bool) {
        self.config.realtime.auto_start = auto_start;
    }

    /// Ask the app to auto-start voice once the thread exists. The app decides
    /// whether this is the first eligible thread of the process.
    pub(in crate::chatwidget) fn request_realtime_auto_start(&self) {
        if self.config.realtime.auto_start
            && let Some(thread_id) = self.thread_id()
        {
            self.app_event_tx
                .send(crate::app_event::AppEvent::VoiceControl {
                    thread_id: Some(thread_id),
                    control: crate::app_event::VoiceControl::AutoStart,
                });
        }
    }

    pub(super) fn voice_status_available(&self) -> bool {
        self.realtime_conversation.phase == RealtimeConversationPhase::Active
            && !self.app_event_tx.voice_only.load(Ordering::Relaxed)
            && self.realtime_conversation.thread_id.is_some()
            && self.thread_id() == self.realtime_conversation.thread_id
    }

    pub(crate) fn is_realtime_status_speech(&self, delivery_id: u64) -> bool {
        self.realtime_conversation
            .voice_controls
            .status_speech
            .contains(&delivery_id)
    }

    pub(super) fn forget_realtime_status_speech(&mut self, delivery_id: u64) -> bool {
        let before = self
            .realtime_conversation
            .voice_controls
            .status_speech
            .len();
        self.realtime_conversation
            .voice_controls
            .status_speech
            .retain(|id| *id != delivery_id);
        before
            != self
                .realtime_conversation
                .voice_controls
                .status_speech
                .len()
    }

    /// Speak a short status line through the call's appendSpeech path, or
    /// through local text-to-speech in a local voice session.
    pub(super) fn speak_realtime_status(&mut self, text: &str) -> bool {
        if !self.voice_status_available() {
            return false;
        }
        let Some(thread_id) = self.realtime_conversation.thread_id else {
            return false;
        };
        let text = limit_words(text, MAX_SPOKEN_WORDS);
        if text.is_empty() {
            return false;
        }
        if self.local_voice_running() {
            return self.speak_local_voice_status(&text);
        }
        let delivery_id = NEXT_REALTIME_SPEECH_DELIVERY_ID.fetch_add(1, Ordering::Relaxed);
        if self
            .realtime_conversation
            .voice_controls
            .status_speech
            .len()
            >= MAX_STATUS_SPEECH_DELIVERIES
        {
            self.realtime_conversation
                .voice_controls
                .status_speech
                .pop_front();
        }
        self.realtime_conversation
            .voice_controls
            .status_speech
            .push_back(delivery_id);
        let submitted = self.submit_op(AppCommand::RealtimeConversationSpeech {
            thread_id,
            attempt_id: self.realtime_conversation.attempt_id,
            input_generation: self.realtime_conversation.input_generation,
            delivery_id,
            text: text.into(),
        });
        if !submitted {
            self.forget_realtime_status_speech(delivery_id);
        }
        submitted
    }

    /// Describe the visible approval prompt aloud during a voice conversation.
    pub(crate) fn announce_realtime_approval_request(&mut self) {
        if !self.voice_status_available() {
            return;
        }
        let Some(prompt) = self
            .bottom_pane
            .current_approval_request()
            .map(approval_request_prompt)
        else {
            return;
        };
        // A new prompt never inherits a half-finished voice approval.
        self.realtime_conversation.voice_controls.voice_approval = None;
        // Nor waits behind a long spoken answer.
        self.interrupt_local_voice_answer();
        self.speak_realtime_status(&prompt);
    }

    /// Drop a half-finished spoken approval ("approve" said, "confirm" not yet)
    /// of a prompt that was answered elsewhere or withdrawn.
    pub(in crate::chatwidget) fn forget_voice_approval_of(
        &mut self,
        request: &ResolvedAppServerRequest,
    ) {
        let Some(key) = resolved_voice_key(request) else {
            return;
        };
        let voice_controls = &mut self.realtime_conversation.voice_controls;
        if voice_controls
            .voice_approval
            .as_ref()
            .is_some_and(|pending| pending.request_key == key)
        {
            voice_controls.voice_approval = None;
        }
    }

    /// Show a note about approvals on another machine, and say it during a
    /// voice conversation ("Couldn't reach devbox; the approval is still
    /// waiting there.").
    pub(crate) fn notify_remote_approval_status(&mut self, text: &str) {
        self.add_info_message(text.to_string(), /*hint*/ None);
        self.speak_realtime_status(text);
        self.request_redraw();
    }

    /// Summarize a failed turn aloud in one short sentence.
    pub(in crate::chatwidget) fn speak_realtime_turn_error(&mut self, message: &str) {
        if self.thread_usage.replaying_turn_completion {
            return;
        }
        self.realtime_conversation.voice_controls.voice_approval = None;
        self.speak_realtime_status(&turn_error_speech(message));
    }

    /// Route a final user transcript. Approval answers are applied locally;
    /// voice commands go to the app's orchestrator. Approvals always come
    /// first, so a bare "stop" stays an approval abort.
    ///
    /// Returns `true` when the transcript was handled here and is not a
    /// request for the model.
    pub(super) fn on_realtime_user_utterance(
        &mut self,
        text: &str,
        pre_utterance_generation: u64,
    ) -> bool {
        let intent = parse_voice_approval_intent(text);
        if let Some(intent) = intent
            && self.take_consumed_voice_control(VoiceControlSource::Handoff, intent)
        {
            // The model's handoff of these same words already applied them.
            self.keep_in_flight_voice_turns(pre_utterance_generation);
            return true;
        }
        if self.handle_realtime_voice_approval(text) {
            if let Some(intent) = intent {
                self.remember_consumed_voice_control(VoiceControlSource::Transcript, intent);
            }
            self.keep_in_flight_voice_turns(pre_utterance_generation);
            true
        } else if self.handle_realtime_voice_command(text) {
            // Approval echoes stay remembered: a voice command is not new speech
            // for the model, so a late approval handoff is still recognized.
            self.keep_in_flight_voice_turns(pre_utterance_generation);
            true
        } else {
            // A later handoff of ordinary speech must not be mistaken for an old answer.
            self.realtime_conversation
                .voice_controls
                .consumed_voice_controls
                .clear();
            false
        }
    }

    /// A handoff that only repeats an approval answer or a voice command is not a
    /// new voice request.
    ///
    /// Returns `true` when the delegated input was answered locally or swallowed.
    /// Its turn keeps any existing voice ownership and never takes over speech.
    pub(super) fn consume_realtime_voice_control_handoff(
        &mut self,
        turn_id: &str,
        input: &str,
    ) -> bool {
        if self.realtime_conversation.phase != RealtimeConversationPhase::Active {
            return false;
        }
        let display = realtime_delegation_display_text(input);
        let handled = match parse_voice_approval_intent(&display) {
            Some(intent) => self.consume_realtime_approval_handoff(&display, intent),
            None => false,
        };
        // A voice command, or a "confirm"/"yes"/"no" while one waits, is swallowed
        // and never run: only the user's own transcript runs voice commands.
        let handled = handled || self.swallow_realtime_voice_command_handoff(&display);
        if !handled {
            return false;
        }
        let input_generation = self.realtime_conversation.input_generation;
        self.realtime_conversation
            .turn_origins
            .entry(turn_id.to_string())
            .or_insert(RealtimeTurnOrigin::Delegated {
                may_speak: false,
                input_generation,
            });
        true
    }

    /// Lane A: a handed-off approval answer. Returns `true` when it was handled.
    fn consume_realtime_approval_handoff(
        &mut self,
        display: &str,
        intent: VoiceApprovalIntent,
    ) -> bool {
        if self.take_consumed_voice_control(VoiceControlSource::Transcript, intent) {
            // The user's transcript already answered; this is its echo.
            true
        } else {
            match intent {
                // A refusal is safe to apply from the handoff. Remember it so the
                // late transcript of the same words does not refuse the next prompt.
                VoiceApprovalIntent::Deny | VoiceApprovalIntent::Abort => {
                    let applied = self.handle_realtime_voice_approval(display);
                    if applied {
                        self.remember_consumed_voice_control(VoiceControlSource::Handoff, intent);
                    }
                    applied
                }
                // Only the user's own transcript may arm or confirm an approval,
                // so a model-authored "confirm" can never approve anything. While
                // a prompt is open, hold the handoff back and let the transcript act.
                VoiceApprovalIntent::Approve
                | VoiceApprovalIntent::ApproveForSession
                | VoiceApprovalIntent::Confirm => {
                    self.voice_status_available()
                        && self.bottom_pane.current_approval_request().is_some()
                }
            }
        }
    }

    fn remember_consumed_voice_control(
        &mut self,
        source: VoiceControlSource,
        intent: VoiceApprovalIntent,
    ) {
        let consumed = &mut self
            .realtime_conversation
            .voice_controls
            .consumed_voice_controls;
        if consumed.len() >= MAX_CONSUMED_VOICE_CONTROLS {
            consumed.pop_front();
        }
        consumed.push_back(ConsumedVoiceControl { source, intent });
    }

    /// Remove and report one answer of `intent` previously handled by `source`.
    fn take_consumed_voice_control(
        &mut self,
        source: VoiceControlSource,
        intent: VoiceApprovalIntent,
    ) -> bool {
        let consumed = &mut self
            .realtime_conversation
            .voice_controls
            .consumed_voice_controls;
        let wanted = ConsumedVoiceControl { source, intent };
        match consumed.iter().position(|seen| *seen == wanted) {
            Some(index) => {
                consumed.remove(index);
                true
            }
            None => false,
        }
    }

    /// Speaking an approval answer must not orphan the answer of the voice turn
    /// that is waiting on it.
    fn keep_in_flight_voice_turns(&mut self, pre_utterance_generation: u64) {
        let current = self.realtime_conversation.input_generation;
        if current == pre_utterance_generation {
            return;
        }
        for origin in self.realtime_conversation.turn_origins.values_mut() {
            if let RealtimeTurnOrigin::Delegated {
                may_speak: true,
                input_generation,
            } = origin
                && *input_generation == pre_utterance_generation
            {
                *input_generation = current;
            }
        }
        for origin in self.realtime_conversation.agent_items.values_mut() {
            if let RealtimeAgentItemOrigin::Delegated {
                may_speak: true,
                input_generation,
                ..
            } = origin
                && *input_generation == pre_utterance_generation
            {
                *input_generation = current;
            }
        }
        for pending in &mut self.realtime_conversation.pending_speech {
            if pending.state == super::PendingSpeechState::AwaitingTurn
                && pending.input_generation == pre_utterance_generation
            {
                pending.input_generation = current;
            }
        }
    }

    /// Apply an approval answer spoken while an approval prompt is visible.
    ///
    /// Returns `true` when the utterance was an approval answer and must not be
    /// treated as a request for the model.
    fn handle_realtime_voice_approval(&mut self, utterance: &str) -> bool {
        if !self.voice_status_available() {
            return false;
        }
        let Some((request_key, confirmation_prompts)) =
            self.bottom_pane.current_approval_request().map(|request| {
                (
                    request.voice_key(),
                    [
                        approval_confirmation_prompt(request, VoiceApprovalIntent::Approve),
                        approval_confirmation_prompt(
                            request,
                            VoiceApprovalIntent::ApproveForSession,
                        ),
                    ],
                )
            })
        else {
            self.realtime_conversation.voice_controls.voice_approval = None;
            return false;
        };
        let pending = self
            .realtime_conversation
            .voice_controls
            .voice_approval
            .take()
            .filter(|pending| pending.request_key == request_key);
        // "Option 4", "d" or "continue without permissions": the prompt's own
        // options, by number, letter or label.
        let options = self.bottom_pane.voice_approval_options();
        if let Some(index) = parse_voice_approval_option(utterance, &options) {
            let option = &options[index];
            if option.approves {
                let prompt = approval_option_confirmation_prompt(index, option);
                self.realtime_conversation.voice_controls.voice_approval =
                    Some(PendingVoiceApproval {
                        request_key,
                        intent: VoiceApprovalIntent::Approve,
                        option: Some(index),
                    });
                self.add_info_message(prompt.clone(), /*hint*/ None);
                self.speak_realtime_status(&prompt);
            } else if self.bottom_pane.apply_voice_approval_option(index) {
                self.speak_realtime_status(&format!("{}.", approval_option_phrase(index, option)));
                self.after_voice_approval_decision();
            } else {
                self.speak_realtime_status("That option is no longer available.");
            }
            self.request_redraw();
            return true;
        }
        let Some(intent) = parse_voice_approval_intent(utterance) else {
            if pending.is_some() {
                self.add_info_message(
                    "Voice approval cancelled; the approval prompt is still open.".to_string(),
                    /*hint*/ None,
                );
                self.speak_realtime_status("Approval cancelled.");
            }
            return false;
        };
        match intent {
            VoiceApprovalIntent::Confirm => {
                let Some(pending) = pending else {
                    self.speak_realtime_status("Say approve first, then confirm.");
                    return true;
                };
                let applied = match pending.option {
                    Some(index) => self.bottom_pane.apply_voice_approval_option(index),
                    None => match decision_for(pending.intent) {
                        Some(decision) => self.bottom_pane.apply_voice_approval(decision),
                        None => return true,
                    },
                };
                if applied {
                    self.after_voice_approval_decision();
                } else {
                    self.speak_realtime_status("That option is no longer available.");
                }
            }
            VoiceApprovalIntent::Approve | VoiceApprovalIntent::ApproveForSession => {
                let Some(decision) = decision_for(intent) else {
                    return true;
                };
                if !self.bottom_pane.offers_voice_approval(decision) {
                    self.speak_realtime_status("That option is not offered. Say approve or deny.");
                    return true;
                }
                let prompt = if intent == VoiceApprovalIntent::ApproveForSession {
                    confirmation_prompts[1].clone()
                } else {
                    confirmation_prompts[0].clone()
                };
                self.realtime_conversation.voice_controls.voice_approval =
                    Some(PendingVoiceApproval {
                        request_key,
                        intent,
                        option: None,
                    });
                self.add_info_message(prompt.clone(), /*hint*/ None);
                self.speak_realtime_status(&prompt);
            }
            VoiceApprovalIntent::Deny | VoiceApprovalIntent::Abort => {
                let Some(decision) = decision_for(intent) else {
                    return true;
                };
                if self.bottom_pane.apply_voice_approval(decision) {
                    self.speak_realtime_status(if intent == VoiceApprovalIntent::Abort {
                        "Stopped."
                    } else {
                        "Denied."
                    });
                    self.after_voice_approval_decision();
                } else {
                    self.speak_realtime_status("That option is no longer available.");
                }
            }
        }
        self.request_redraw();
        true
    }

    fn after_voice_approval_decision(&mut self) {
        self.realtime_conversation.voice_controls.voice_approval = None;
        if self.bottom_pane.current_approval_request().is_some() {
            self.announce_realtime_approval_request();
        }
    }
}
