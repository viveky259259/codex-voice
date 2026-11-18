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
use super::realtime_input_fingerprint;
use super::realtime_voice_supported;
use super::voice_approval::MAX_SPOKEN_WORDS;
use super::voice_approval::VoiceApprovalIntent;
use super::voice_approval::approval_confirmation_prompt;
use super::voice_approval::approval_request_prompt;
use super::voice_approval::limit_words;
use super::voice_approval::parse_voice_approval_intent;
use super::voice_approval::turn_error_speech;
use crate::app_command::AppCommand;
use crate::bottom_pane::VoiceApprovalDecision;
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
    /// Utterances answered locally; a matching handoff is not a new voice request.
    pub(super) consumed_voice_controls: VecDeque<(usize, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingVoiceApproval {
    request_key: String,
    intent: VoiceApprovalIntent,
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
        if !realtime_voice_supported() {
            self.add_info_message(
                VOICE_UNSUPPORTED_AUTO_START_NOTICE.to_string(),
                /*hint*/ None,
            );
            return false;
        }
        if self.config.realtime.session_type != RealtimeWsMode::Conversational {
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

    fn voice_status_available(&self) -> bool {
        self.realtime_conversation.phase == RealtimeConversationPhase::Active
            && !self.app_event_tx.voice_only.load(Ordering::Relaxed)
            && self.realtime_conversation.thread_id.is_some()
            && self.thread_id() == self.realtime_conversation.thread_id
    }

    pub(crate) fn is_realtime_status_speech(&self, delivery_id: u64) -> bool {
        self.realtime_conversation
            .voice_controls.status_speech
            .contains(&delivery_id)
    }

    pub(super) fn forget_realtime_status_speech(&mut self, delivery_id: u64) -> bool {
        let before = self.realtime_conversation.voice_controls.status_speech.len();
        self.realtime_conversation
            .voice_controls.status_speech
            .retain(|id| *id != delivery_id);
        before != self.realtime_conversation.voice_controls.status_speech.len()
    }

    /// Speak a short status line through the call's appendSpeech path.
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
        let delivery_id = NEXT_REALTIME_SPEECH_DELIVERY_ID.fetch_add(1, Ordering::Relaxed);
        if self.realtime_conversation.voice_controls.status_speech.len() >= MAX_STATUS_SPEECH_DELIVERIES {
            self.realtime_conversation.voice_controls.status_speech.pop_front();
        }
        self.realtime_conversation
            .voice_controls.status_speech
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
        self.speak_realtime_status(&prompt);
    }

    /// Summarize a failed turn aloud in one short sentence.
    pub(in crate::chatwidget) fn speak_realtime_turn_error(&mut self, message: &str) {
        if self.thread_usage.replaying_turn_completion {
            return;
        }
        self.realtime_conversation.voice_controls.voice_approval = None;
        self.speak_realtime_status(&turn_error_speech(message));
    }

    /// Route a final user transcript. Approval answers are applied locally.
    pub(super) fn on_realtime_user_utterance(&mut self, text: &str, pre_utterance_generation: u64) {
        if self.handle_realtime_voice_approval(text) {
            self.remember_consumed_voice_control(text);
            self.keep_in_flight_voice_turns(pre_utterance_generation);
        } else {
            // A later handoff of ordinary speech must not be mistaken for an old answer.
            self.realtime_conversation.voice_controls.consumed_voice_controls.clear();
        }
    }

    /// A handoff that only repeats an approval answer is not a new voice request.
    ///
    /// Returns `true` when the delegated input was answered locally. Its turn keeps
    /// any existing voice ownership and never takes over speech.
    pub(super) fn consume_realtime_voice_control_handoff(
        &mut self,
        turn_id: &str,
        input: &str,
    ) -> bool {
        if self.realtime_conversation.phase != RealtimeConversationPhase::Active {
            return false;
        }
        let display = realtime_delegation_display_text(input);
        if parse_voice_approval_intent(&display).is_none() {
            return false;
        }
        let fingerprint = realtime_input_fingerprint(&display);
        let consumed = &mut self.realtime_conversation.voice_controls.consumed_voice_controls;
        let already_answered =
            if let Some(index) = consumed.iter().position(|seen| *seen == fingerprint) {
                consumed.remove(index);
                true
            } else {
                false
            };
        if !already_answered && !self.handle_realtime_voice_approval(&display) {
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

    fn remember_consumed_voice_control(&mut self, text: &str) {
        let consumed = &mut self.realtime_conversation.voice_controls.consumed_voice_controls;
        if consumed.len() >= MAX_CONSUMED_VOICE_CONTROLS {
            consumed.pop_front();
        }
        consumed.push_back(realtime_input_fingerprint(text));
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
            .voice_controls.voice_approval
            .take()
            .filter(|pending| pending.request_key == request_key);
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
                let Some(decision) = decision_for(pending.intent) else {
                    return true;
                };
                if self.bottom_pane.apply_voice_approval(decision) {
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
                self.realtime_conversation.voice_controls.voice_approval = Some(PendingVoiceApproval {
                    request_key,
                    intent,
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
