//! `[realtime] backend = "local"`: voice through `codex-local-voice` instead of
//! OpenAI's realtime service.
//!
//! The engine captures the microphone, detects speech, and transcribes it
//! locally. Each final transcript takes the same route as a realtime user
//! transcript: approval answers first, then voice commands, and anything else
//! is submitted exactly as if it had been typed and entered in the composer.
//! With `[realtime] send_delay_ms` (5 s by default) a finished utterance is
//! held first, with a countdown in the voice strip: speaking again adds to it
//! and restarts the wait, "send" sends it at once and "cancel" drops it.
//! Spoken status prompts and the short summary of each final answer (typed
//! requests included; `[realtime] speak_answers = false` turns answers off)
//! go to the engine's local text-to-speech.
//!
//! There is no voice model in this mode, so no handoffs: approvals are only
//! ever answered from the user's own transcript.
//!
//! The engine sits behind [`LocalVoiceEngine`] so tests can inject events and
//! record speech without audio.

use super::super::ChatWidget;
use super::NEXT_REALTIME_ATTEMPT_ID;
use super::RealtimeConversationPhase;
use crate::app_event::AppEvent;
use crate::bottom_pane::HeldVoiceMessage;
use crate::chatwidget::ShellEscapePolicy;
use crate::chatwidget::UserMessage;
use crate::history_cell;
use codex_config::config_toml::RealtimeBackend;
use codex_local_voice::LocalVoice;
use codex_local_voice::LocalVoiceConfig;
use codex_local_voice::LocalVoiceEvent;
use codex_local_voice::LocalVoiceHandle;
use codex_local_voice::VoiceStage;
use futures::future::BoxFuture;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Spoken answers keep to this many sentences...
const MAX_SPOKEN_ANSWER_SENTENCES: usize = 2;
/// ...and this many words.
const MAX_SPOKEN_ANSWER_WORDS: usize = 40;
/// Appended when an answer was shortened or had code left out.
pub(crate) const DETAILS_ON_SCREEN: &str = "Details are on screen.";
/// Voice transcripts submitted but not yet seen as a user item.
const MAX_PENDING_VOICE_INPUTS: usize = 8;
const MAX_ERROR_DETAIL_CHARS: usize = 300;
/// How often a held message's countdown updates.
const SEND_TICK: Duration = Duration::from_secs(1);

/// A finished utterance waiting out the send delay, so the user can go on.
#[derive(Debug)]
struct HeldVoiceInput {
    text: String,
    /// When it is sent; `None` while the user is speaking again.
    deadline: Option<Instant>,
}

impl HeldVoiceInput {
    fn seconds_left(&self, now: Instant) -> Option<u64> {
        self.deadline.map(|deadline| {
            let left = deadline.saturating_duration_since(now).as_millis();
            u64::try_from(left.div_ceil(1000)).unwrap_or(u64::MAX)
        })
    }
}

/// What a short utterance asks of a held message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeldInputCommand {
    Send,
    Cancel,
}

/// "Send it." sends a held message at once; "Cancel." or "Never mind." drops
/// it. Only whole utterances match, so "send the email to Sam" is a message.
pub(crate) fn held_input_command(text: &str) -> Option<HeldInputCommand> {
    let normalized = text
        .to_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch == '\'' {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>();
    let words = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    match words.as_str() {
        "send" | "send it" | "send that" | "send now" | "send it now" | "send this"
        | "send the message" | "submit" | "go" => Some(HeldInputCommand::Send),
        "cancel" | "cancel it" | "cancel that" | "cancel this" | "cancel the message"
        | "never mind" | "nevermind" | "scratch that" | "don't send" | "don't send it"
        | "do not send" | "do not send it" | "delete that" | "discard" | "discard it"
        | "discard that" => Some(HeldInputCommand::Cancel),
        _ => None,
    }
}

/// The controls of a running local voice engine.
pub(crate) trait LocalVoiceEngine: Send + Sync {
    /// Queue `text` to be spoken after anything already queued.
    fn speak(&self, text: &str);
    /// Stop the current speech and drop the queue.
    fn stop_speaking(&self);
    /// Drop (or resume) microphone audio.
    fn set_muted(&self, muted: bool);
    /// Kill and reap the engine's children (capture, STT, player).
    fn shutdown(&self) -> BoxFuture<'static, ()>;
}

impl LocalVoiceEngine for LocalVoiceHandle {
    fn speak(&self, text: &str) {
        LocalVoiceHandle::speak(self, text);
    }

    fn stop_speaking(&self) {
        LocalVoiceHandle::stop_speaking(self);
    }

    fn set_muted(&self, muted: bool) {
        LocalVoiceHandle::set_muted(self, muted);
    }

    fn shutdown(&self) -> BoxFuture<'static, ()> {
        let handle = self.clone();
        Box::pin(async move { handle.shutdown().await })
    }
}

pub(crate) type LocalVoiceStart = (Arc<dyn LocalVoiceEngine>, mpsc::Receiver<LocalVoiceEvent>);

#[cfg(test)]
type LocalVoiceStarter = std::rc::Rc<dyn Fn(LocalVoiceConfig) -> LocalVoiceStart>;

#[cfg(test)]
thread_local! {
    static LOCAL_VOICE_STARTER: std::cell::RefCell<Option<LocalVoiceStarter>> =
        const { std::cell::RefCell::new(None) };
}

/// Replace the real engine on this test thread.
#[cfg(test)]
pub(crate) fn set_local_voice_starter_for_test(
    starter: Option<impl Fn(LocalVoiceConfig) -> LocalVoiceStart + 'static>,
) {
    LOCAL_VOICE_STARTER.with(|cell| {
        *cell.borrow_mut() = starter.map(|starter| std::rc::Rc::new(starter) as LocalVoiceStarter);
    });
}

fn start_engine(config: LocalVoiceConfig) -> LocalVoiceStart {
    #[cfg(test)]
    if let Some(starter) = LOCAL_VOICE_STARTER.with(|cell| cell.borrow().clone()) {
        return starter(config);
    }
    let (handle, events) = LocalVoice::start(config);
    (Arc::new(handle), events)
}

/// The `[realtime.local]` table as the engine's configuration.
pub(crate) fn local_voice_config(
    local: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<LocalVoiceConfig, String> {
    match local {
        None => Ok(LocalVoiceConfig::default()),
        Some(table) => serde_json::from_value(serde_json::Value::Object(table.clone()))
            .map_err(|err| err.to_string()),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LocalActivity {
    #[default]
    Listening,
    Hearing,
    Transcribing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalSpeechKind {
    /// Approval prompts, outcomes, voice-command replies, errors.
    Status,
    /// The summary of a voice turn's final answer.
    Answer,
}

/// One local voice session; dropped (and shut down) with the conversation.
pub(crate) struct LocalVoiceSession {
    engine: Arc<dyn LocalVoiceEngine>,
    forwarder: Option<JoinHandle<()>>,
    activity: LocalActivity,
    speaking: bool,
    microphone_failed: bool,
    /// The capture command exited cleanly (e.g. a file fed in as the mic ended).
    microphone_ended: bool,
    /// What is queued or playing, oldest first.
    speech: VecDeque<LocalSpeechKind>,
    /// Transcripts submitted as user turns whose user item has not arrived.
    pending_voice_inputs: VecDeque<String>,
    /// Turns whose latest user input was spoken; their final answer is spoken.
    voice_turns: HashSet<String>,
    /// Error notices already shown in this session.
    reported_errors: HashSet<String>,
    /// The spoken message waiting out `send_delay_ms`.
    held: Option<HeldVoiceInput>,
    /// Identifies the running countdown; ticks of older ones are ignored.
    countdown: u64,
}

impl LocalVoiceSession {
    fn new(engine: Arc<dyn LocalVoiceEngine>, forwarder: Option<JoinHandle<()>>) -> Self {
        Self {
            engine,
            forwarder,
            activity: LocalActivity::default(),
            speaking: false,
            microphone_failed: false,
            microphone_ended: false,
            speech: VecDeque::new(),
            pending_voice_inputs: VecDeque::new(),
            voice_turns: HashSet::new(),
            reported_errors: HashSet::new(),
            held: None,
            countdown: 0,
        }
    }

    /// The held message as the voice strip shows it.
    pub(super) fn held_message(&self, now: Instant) -> Option<HeldVoiceMessage> {
        self.held.as_ref().map(|held| HeldVoiceMessage {
            text: held.text.clone(),
            seconds_left: held.seconds_left(now),
        })
    }

    pub(super) fn activity_label(&self) -> &'static str {
        if self.speaking {
            "speaking"
        } else if self.microphone_failed {
            "mic unavailable"
        } else if self.microphone_ended {
            "mic off"
        } else {
            match self.activity {
                LocalActivity::Listening => "listening",
                LocalActivity::Hearing => "hearing you",
                LocalActivity::Transcribing => "transcribing",
            }
        }
    }

    fn speak(&mut self, text: &str, kind: LocalSpeechKind) {
        if text.trim().is_empty() {
            return;
        }
        self.engine.speak(text);
        self.speech.push_back(kind);
    }

    pub(super) fn stop_speaking(&mut self) {
        self.engine.stop_speaking();
        self.speech.clear();
    }

    pub(super) fn set_muted(&mut self, muted: bool) {
        self.engine.set_muted(muted);
    }

    pub(super) fn forget_turn(&mut self, turn_id: &str) {
        self.voice_turns.remove(turn_id);
    }

    /// Stop the engine; its children are killed and reaped by the returned future.
    fn into_shutdown(mut self) -> BoxFuture<'static, ()> {
        if let Some(forwarder) = self.forwarder.take() {
            forwarder.abort();
        }
        self.engine.shutdown()
    }

    /// Stop the engine without waiting. Dropping the engine also kills its
    /// children, so this is safe without a runtime.
    pub(super) fn shutdown_in_background(self) {
        let shutdown = self.into_shutdown();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(shutdown);
        }
    }
}

impl ChatWidget {
    /// Whether `[realtime] backend = "local"` is configured.
    pub(crate) fn local_voice_selected(&self) -> bool {
        self.config.realtime.backend == RealtimeBackend::Local
    }

    /// Whether this widget runs a local voice session.
    pub(crate) fn local_voice_running(&self) -> bool {
        self.realtime_conversation.local.is_some()
    }

    #[cfg(test)]
    pub(crate) fn set_realtime_backend_for_test(&mut self, backend: RealtimeBackend) {
        self.config.realtime.backend = backend;
    }

    #[cfg(test)]
    pub(crate) fn local_voice_attempt_for_test(&self) -> u64 {
        self.realtime_conversation.attempt_id
    }

    #[cfg(test)]
    pub(crate) fn local_voice_activity_for_test(&self) -> Option<&'static str> {
        self.realtime_conversation
            .local
            .as_ref()
            .map(|local| local.activity_label())
    }

    /// Start a local voice session: no realtime service, no voice host.
    pub(super) fn start_local_voice_conversation(&mut self, thread_id: codex_protocol::ThreadId) {
        let config = match local_voice_config(self.config.realtime.local.as_ref()) {
            Ok(config) => config,
            Err(err) => {
                self.add_error_message(format!(
                    "Local voice settings in [realtime.local] are invalid: {err}"
                ));
                return;
            }
        };
        self.realtime_conversation.recover_late_transcripts = false;
        self.realtime_conversation.attempt_id =
            NEXT_REALTIME_ATTEMPT_ID.fetch_add(1, Ordering::Relaxed);
        let attempt_id = self.realtime_conversation.attempt_id;
        self.realtime_conversation.thread_id = Some(thread_id);

        let (engine, mut events) = start_engine(config);
        let forwarder = tokio::runtime::Handle::try_current().ok().map(|runtime| {
            let app_event_tx = self.app_event_tx.clone();
            runtime.spawn(async move {
                while let Some(event) = events.recv().await {
                    app_event_tx.send(AppEvent::LocalVoice {
                        thread_id,
                        attempt_id,
                        event,
                    });
                }
            })
        });
        if self.realtime_conversation.microphone_muted {
            engine.set_muted(true);
        }
        self.realtime_conversation.local =
            Some(Box::new(LocalVoiceSession::new(engine, forwarder)));

        // Local voice is ready at once: activate through the same state the
        // realtime call reaches, so approvals and voice commands work unchanged.
        self.realtime_conversation.phase = RealtimeConversationPhase::Active;
        self.realtime_conversation.active_since = Some(Instant::now());
        self.realtime_conversation.latest_input_was_voice = true;
        self.session_telemetry
            .counter("codex.voice.session.connected", /*inc*/ 1, &[]);
        self.update_realtime_footer();
        self.refresh_terminal_title();
        self.add_info_message(
            "Voice conversation started (local speech).".to_string(),
            Some("Use /voice mute to mute or /voice to stop.".to_string()),
        );
        self.request_voice_command_refresh();
        self.app_event_tx
            .send(AppEvent::RealtimeConversationStateChanged);
        self.request_redraw();
    }

    /// Detach the local engine and return the future that stops it, so the
    /// app can wait for its children to exit (quit, `/voice stop`).
    pub(crate) fn take_local_voice_shutdown(&mut self) -> Option<BoxFuture<'static, ()>> {
        self.realtime_conversation
            .local
            .take()
            .map(|local| local.into_shutdown())
    }

    /// An event from this widget's local voice engine.
    pub(crate) fn on_local_voice_event(&mut self, attempt_id: u64, event: LocalVoiceEvent) {
        if self.realtime_conversation.attempt_id != attempt_id
            || self.realtime_conversation.phase != RealtimeConversationPhase::Active
        {
            return;
        }
        let Some(local) = self.realtime_conversation.local.as_mut() else {
            return;
        };
        match event {
            LocalVoiceEvent::Listening => {
                local.activity = LocalActivity::Listening;
                local.microphone_failed = false;
                local.microphone_ended = false;
                // Speech that produced no transcript: the countdown resumes.
                if local
                    .held
                    .as_ref()
                    .is_some_and(|held| held.deadline.is_none())
                {
                    self.restart_local_voice_countdown();
                }
            }
            LocalVoiceEvent::SpeechStarted => {
                local.activity = LocalActivity::Hearing;
                // Still talking: hold the message until this utterance ends.
                if let Some(held) = local.held.as_mut() {
                    held.deadline = None;
                }
            }
            LocalVoiceEvent::SpeechEnded => local.activity = LocalActivity::Transcribing,
            LocalVoiceEvent::SpeakingStarted => local.speaking = true,
            LocalVoiceEvent::SpeakingFinished => {
                local.speaking = false;
                local.speech.pop_front();
            }
            LocalVoiceEvent::Transcript { text } => {
                local.activity = LocalActivity::Listening;
                self.on_local_voice_transcript(&text);
            }
            LocalVoiceEvent::CaptureEnded { message } => {
                local.microphone_ended = true;
                tracing::info!("local voice: {message}");
                self.add_info_message(
                    "Voice input ended: the capture command exited. Press F8 twice to restart voice."
                        .to_string(),
                    /*hint*/ None,
                );
            }
            LocalVoiceEvent::Error { stage, message } => {
                if stage == VoiceStage::Tts {
                    // Synthesis failed before anything played.
                    local.speech.pop_front();
                }
                if stage == VoiceStage::Capture {
                    local.microphone_failed = true;
                }
                self.on_local_voice_error(stage, &message);
            }
        }
        self.update_realtime_footer();
        self.request_redraw();
    }

    /// Route one final transcript: a held message's "send" or "cancel", then
    /// approvals, then voice commands, then a user turn (held first when
    /// `send_delay_ms` is set).
    fn on_local_voice_transcript(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.app_event_tx.voice_only.load(Ordering::Relaxed) {
            // Parked while its thread's view is rebuilt: nothing here can act.
            tracing::debug!("local voice transcript dropped while the voice view is parked");
            return;
        }
        let holding = self
            .realtime_conversation
            .local
            .as_ref()
            .is_some_and(|local| local.held.is_some());
        if holding && let Some(command) = held_input_command(text) {
            match command {
                HeldInputCommand::Send => self.send_held_local_voice_input(),
                HeldInputCommand::Cancel => self.cancel_held_local_voice_input(),
            }
            return;
        }
        let generation = self.realtime_conversation.input_generation;
        if self.on_realtime_user_utterance(text, generation) {
            // Show what was heard; it never reaches the model.
            self.add_to_history(history_cell::new_spoken_user_prompt(text.to_string()));
            return;
        }
        if self.refuse_local_voice_input_during_approval(text) {
            return;
        }
        let delay = Duration::from_millis(self.config.realtime.send_delay_ms);
        if delay.is_zero() {
            self.submit_local_voice_input(text);
            return;
        }
        if let Some(local) = self.realtime_conversation.local.as_mut() {
            match local.held.as_mut() {
                Some(held) => {
                    held.text.push(' ');
                    held.text.push_str(text);
                }
                None => {
                    local.held = Some(HeldVoiceInput {
                        text: text.to_string(),
                        deadline: None,
                    });
                }
            }
        }
        self.restart_local_voice_countdown();
    }

    /// Typing cannot reach the model while an approval prompt is open either;
    /// never steer a turn that waits on the user.
    fn refuse_local_voice_input_during_approval(&mut self, text: &str) -> bool {
        if self.bottom_pane.current_approval_request().is_none() {
            return false;
        }
        self.add_to_history(history_cell::new_spoken_user_prompt(text.to_string()));
        self.add_info_message(
            "Not sent: an approval is waiting. Say approve or deny.".to_string(),
            /*hint*/ None,
        );
        self.speak_realtime_status("Say approve or deny.");
        true
    }

    /// Submit a spoken message as if it had been typed and entered.
    fn submit_local_voice_input(&mut self, text: &str) {
        if let Some(local) = self.realtime_conversation.local.as_mut() {
            if local.pending_voice_inputs.len() >= MAX_PENDING_VOICE_INPUTS {
                local.pending_voice_inputs.pop_front();
            }
            local.pending_voice_inputs.push_back(text.to_string());
        }
        self.submit_entered_user_message(
            UserMessage {
                text: text.to_string(),
                local_images: Vec::new(),
                remote_image_urls: Vec::new(),
                text_elements: Vec::new(),
                mention_bindings: Vec::new(),
            },
            // A transcript is never a `!` shell command.
            ShellEscapePolicy::Disallow,
        );
    }

    /// Start the held message's full wait again and tick its countdown.
    fn restart_local_voice_countdown(&mut self) {
        let delay = Duration::from_millis(self.config.realtime.send_delay_ms);
        let Some(local) = self.realtime_conversation.local.as_mut() else {
            return;
        };
        let Some(held) = local.held.as_mut() else {
            return;
        };
        held.deadline = Some(Instant::now() + delay);
        local.countdown += 1;
        let countdown = local.countdown;
        self.schedule_local_voice_send_tick(countdown, delay.min(SEND_TICK));
    }

    fn schedule_local_voice_send_tick(&self, countdown: u64, after: Duration) {
        let (Some(thread_id), Ok(runtime)) = (
            self.realtime_conversation.thread_id,
            tokio::runtime::Handle::try_current(),
        ) else {
            return;
        };
        let attempt_id = self.realtime_conversation.attempt_id;
        let app_event_tx = self.app_event_tx.clone();
        runtime.spawn(async move {
            tokio::time::sleep(after).await;
            app_event_tx.send(AppEvent::LocalVoiceSendTick {
                thread_id,
                attempt_id,
                countdown,
            });
        });
    }

    /// A countdown tick: send the held message once its wait is over.
    pub(crate) fn on_local_voice_send_tick(&mut self, attempt_id: u64, countdown: u64) {
        if self.realtime_conversation.attempt_id != attempt_id
            || self.realtime_conversation.phase != RealtimeConversationPhase::Active
        {
            return;
        }
        let Some(local) = self.realtime_conversation.local.as_ref() else {
            return;
        };
        if local.countdown != countdown {
            return;
        }
        let Some(deadline) = local.held.as_ref().and_then(|held| held.deadline) else {
            // Paused while the user speaks; the next utterance restarts it.
            return;
        };
        let now = Instant::now();
        if now >= deadline && !self.app_event_tx.voice_only.load(Ordering::Relaxed) {
            self.send_held_local_voice_input();
        } else {
            let left = deadline.saturating_duration_since(now);
            let until_next_second =
                Duration::from_millis(u64::try_from(left.as_millis() % 1000).unwrap_or(0));
            let after = if until_next_second.is_zero() {
                SEND_TICK
            } else {
                until_next_second
            };
            self.schedule_local_voice_send_tick(countdown, after);
        }
        self.update_realtime_footer();
        self.request_redraw();
    }

    /// Send the held message now.
    fn send_held_local_voice_input(&mut self) {
        let Some(held) = self
            .realtime_conversation
            .local
            .as_mut()
            .and_then(|local| local.held.take())
        else {
            return;
        };
        if !self.refuse_local_voice_input_during_approval(&held.text) {
            self.submit_local_voice_input(&held.text);
        }
        self.update_realtime_footer();
        self.request_redraw();
    }

    /// Drop the held message.
    fn cancel_held_local_voice_input(&mut self) {
        let Some(held) = self
            .realtime_conversation
            .local
            .as_mut()
            .and_then(|local| local.held.take())
        else {
            return;
        };
        self.add_info_message(
            format!("Voice message cancelled: “{}”", held.text),
            /*hint*/ None,
        );
        self.update_realtime_footer();
        self.request_redraw();
    }

    /// A user item started: remember whether the turn's latest input was spoken.
    /// Returns `false` when no local session runs.
    pub(super) fn note_local_voice_user_item(
        &mut self,
        turn_id: &str,
        items: &[codex_app_server_protocol::UserInput],
    ) -> bool {
        let Some(local) = self.realtime_conversation.local.as_mut() else {
            return false;
        };
        let spoken = items.iter().find_map(|item| match item {
            codex_app_server_protocol::UserInput::Text { text, .. } => local
                .pending_voice_inputs
                .iter()
                .position(|pending| pending == text),
            _ => None,
        });
        match spoken {
            Some(index) => {
                local.pending_voice_inputs.remove(index);
                local.voice_turns.insert(turn_id.to_string());
            }
            None => {
                local.voice_turns.remove(turn_id);
            }
        }
        true
    }

    /// Speak a short summary of a completed voice turn's final answer.
    pub(in crate::chatwidget) fn speak_local_voice_answer(&mut self, turn_id: &str, answer: &str) {
        if !self.voice_status_available() {
            return;
        }
        let speak_answers = self.config.realtime.speak_answers;
        let Some(local) = self.realtime_conversation.local.as_mut() else {
            return;
        };
        local.voice_turns.remove(turn_id);
        if !speak_answers {
            return;
        }
        let speech = spoken_answer(answer);
        local.speak(&speech, LocalSpeechKind::Answer);
    }

    /// Speak a status line through local text-to-speech.
    pub(super) fn speak_local_voice_status(&mut self, text: &str) -> bool {
        match self.realtime_conversation.local.as_mut() {
            Some(local) => {
                local.speak(text, LocalSpeechKind::Status);
                true
            }
            None => false,
        }
    }

    /// A new approval prompt interrupts a spoken answer instead of waiting behind it.
    pub(super) fn interrupt_local_voice_answer(&mut self) {
        if let Some(local) = self.realtime_conversation.local.as_mut()
            && local.speech.contains(&LocalSpeechKind::Answer)
        {
            local.stop_speaking();
            local.speaking = false;
        }
    }

    fn on_local_voice_error(&mut self, stage: VoiceStage, message: &str) {
        tracing::warn!(%stage, "local voice: {message}");
        let (key, notice) = local_voice_error_notice(stage, message);
        let Some(local) = self.realtime_conversation.local.as_mut() else {
            return;
        };
        if !local.reported_errors.insert(key) {
            return;
        }
        self.add_error_message(notice);
        match stage {
            VoiceStage::Capture => {
                self.speak_realtime_status("I can't use the microphone. Details are on screen.");
            }
            VoiceStage::Stt => {
                self.speak_realtime_status("Speech recognition failed. Details are on screen.");
            }
            VoiceStage::Config | VoiceStage::Tts | VoiceStage::Playback => {}
        }
    }
}

/// One clear message (with the fix) per kind of local voice failure, and the
/// key that keeps it from repeating.
pub(crate) fn local_voice_error_notice(stage: VoiceStage, message: &str) -> (String, String) {
    let lower = message.to_lowercase();
    if let Some(program) = missing_program(message) {
        let fix = match program.as_str() {
            "ffmpeg" => "Install it with: brew install ffmpeg".to_string(),
            "mlx_whisper" => "Install it with: uv tool install mlx-whisper".to_string(),
            "whisper-cli" => "Install it with: brew install whisper-cpp".to_string(),
            "espeak-ng" | "aplay" => format!("Install `{program}` with your package manager."),
            _ => format!("Install `{program}`, or change the command in [realtime.local]."),
        };
        return (
            format!("missing:{program}"),
            format!(
                "Local voice could not run `{program}` ({stage}): it is not installed or not on PATH. {fix}"
            ),
        );
    }
    if stage == VoiceStage::Capture
        && [
            "permission",
            "not authorized",
            "not permitted",
            "denied",
            "cannot use",
            "input/output error",
        ]
        .iter()
        .any(|hint| lower.contains(hint))
    {
        return (
            "microphone-access".to_string(),
            "Local voice cannot open the microphone. Grant microphone access to your terminal in System Settings → Privacy & Security → Microphone, then restart the terminal and press F8 twice. To pick another input, set [realtime.local.capture] device (list inputs with: ffmpeg -f avfoundation -list_devices true -i \"\").".to_string(),
        );
    }
    let detail: String = message
        .trim()
        .chars()
        .take(MAX_ERROR_DETAIL_CHARS)
        .collect();
    let text = match stage {
        VoiceStage::Config => {
            format!("Local voice settings are invalid: {detail}. Check [realtime.local].")
        }
        VoiceStage::Capture => format!(
            "Local voice stopped listening: {detail}. Check [realtime.local.capture], then press F8 twice."
        ),
        VoiceStage::Stt => {
            format!("Local speech-to-text failed: {detail}. Check [realtime.local.stt].")
        }
        VoiceStage::Tts | VoiceStage::Playback => {
            format!("Local text-to-speech failed: {detail}. Check [realtime.local.tts].")
        }
    };
    (format!("stage:{stage}"), text)
}

/// `failed to start `ffmpeg`: No such file or directory (os error 2)` → `ffmpeg`.
fn missing_program(message: &str) -> Option<String> {
    let lower = message.to_lowercase();
    if !(lower.contains("no such file")
        || lower.contains("not found")
        || lower.contains("os error 2"))
    {
        return None;
    }
    let (_, rest) = message.split_once("failed to start `")?;
    let (program, _) = rest.split_once('`')?;
    let name = program.rsplit('/').next().unwrap_or(program);
    (!name.is_empty()).then(|| name.to_string())
}

/// The spoken form of a final answer: no code blocks or markdown, at most two
/// sentences and about forty words, plus "Details are on screen." when
/// anything was left out.
pub(crate) fn spoken_answer(markdown: &str) -> String {
    let mut omitted = false;
    let mut fence: Option<&str> = None;
    let mut pieces: Vec<String> = Vec::new();
    for line in markdown.lines() {
        let trimmed = line.trim();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        if let Some(marker) = ["```", "~~~"]
            .into_iter()
            .find(|marker| trimmed.starts_with(marker))
        {
            fence = Some(marker);
            omitted = true;
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('|') {
            let cells: Vec<&str> = trimmed
                .split('|')
                .map(str::trim)
                .filter(|cell| !cell.is_empty())
                .collect();
            if cells
                .iter()
                .all(|cell| cell.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
            {
                continue;
            }
            omitted = true;
            let text = strip_inline_markdown(&cells.join(", "));
            if !text.is_empty() {
                pieces.push(text);
            }
            continue;
        }
        let text = strip_inline_markdown(strip_line_prefix(trimmed));
        if !text.is_empty() {
            pieces.push(text);
        }
    }

    let mut joined = String::new();
    for piece in pieces {
        if !joined.is_empty() {
            if !joined.ends_with(['.', '!', '?', ':', ';']) {
                joined.push('.');
            }
            joined.push(' ');
        }
        joined.push_str(&piece);
    }

    let sentences = split_sentences(&joined);
    if sentences.len() > MAX_SPOKEN_ANSWER_SENTENCES {
        omitted = true;
    }
    let kept = sentences
        .into_iter()
        .take(MAX_SPOKEN_ANSWER_SENTENCES)
        .collect::<Vec<_>>()
        .join(" ");
    let words: Vec<&str> = kept.split_whitespace().collect();
    let mut spoken = if words.len() > MAX_SPOKEN_ANSWER_WORDS {
        omitted = true;
        let mut cut = words[..MAX_SPOKEN_ANSWER_WORDS].join(" ");
        while cut.ends_with([',', ';', ':', '-', '—']) {
            cut.pop();
        }
        if !cut.ends_with(['.', '!', '?']) {
            cut.push('.');
        }
        cut
    } else {
        words.join(" ")
    };
    if omitted {
        if !spoken.is_empty() {
            if !spoken.ends_with(['.', '!', '?']) {
                spoken.push('.');
            }
            spoken.push(' ');
        }
        spoken.push_str(DETAILS_ON_SCREEN);
    }
    spoken
}

/// Drop heading, quote, list and task markers.
fn strip_line_prefix(line: &str) -> &str {
    let mut line = line.trim_start_matches('>').trim_start();
    if line.starts_with('#') {
        line = line.trim_start_matches('#').trim_start();
    }
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            line = rest.trim_start();
            break;
        }
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = line[digits..]
            .strip_prefix(". ")
            .or_else(|| line[digits..].strip_prefix(") "))
    {
        line = rest.trim_start();
    }
    for marker in ["[ ] ", "[x] ", "[X] "] {
        if let Some(rest) = line.strip_prefix(marker) {
            line = rest;
            break;
        }
    }
    line
}

/// Keep the words of links, images and inline code; drop emphasis markers and URLs.
fn strip_inline_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        // `[label](target)` and `![alt](target)` keep their label.
        if (ch == '[' || (ch == '!' && chars.get(index + 1) == Some(&'[')))
            && let Some((label, next)) =
                link_label(&chars, if ch == '!' { index + 1 } else { index })
        {
            out.push_str(&label);
            index = next;
            continue;
        }
        match ch {
            '`' | '*' => {}
            '_' if chars.get(index + 1) == Some(&'_') => index += 1,
            '~' if chars.get(index + 1) == Some(&'~') => index += 1,
            _ => out.push(ch),
        }
        index += 1;
    }
    out.split_whitespace()
        .map(|word| {
            if word.starts_with("http://") || word.starts_with("https://") {
                "a link"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// For `[label](target)` starting at `open`, the label and the index after `)`.
fn link_label(chars: &[char], open: usize) -> Option<(String, usize)> {
    let close = open + chars[open..].iter().position(|ch| *ch == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = close + 1 + chars[close + 1..].iter().position(|ch| *ch == ')')?;
    Some((chars[open + 1..close].iter().collect(), end + 1))
}

/// Sentences end at `.`, `!` or `?` followed by whitespace or the end.
fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?') && chars.peek().is_none_or(|next| next.is_whitespace()) {
            let sentence = current.trim().to_string();
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
            current.clear();
        }
    }
    let rest = current.trim();
    if !rest.is_empty() {
        sentences.push(rest.to_string());
    }
    sentences
}

#[cfg(test)]
#[path = "local_voice_tests.rs"]
mod tests;
