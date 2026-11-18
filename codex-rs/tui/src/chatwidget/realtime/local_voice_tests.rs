//! Local voice (`[realtime] backend = "local"`) driven through a fake engine:
//! events are injected and spoken text is recorded, with no audio.

use super::*;
use crate::app_command::AppCommand;
use crate::bottom_pane::ApprovalRequest;
use crate::bottom_pane::ExecApprovalRequest;
use crate::chatwidget::realtime::set_realtime_voice_supported_for_test;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_protocol::models::MessagePhase;
use pretty_assertions::assert_eq;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;

/// Records every call instead of touching audio.
#[derive(Default)]
struct FakeEngine {
    calls: Mutex<Vec<String>>,
}

impl FakeEngine {
    fn record(&self, call: String) {
        self.calls.lock().expect("calls").push(call);
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().expect("calls"))
    }

    fn take_spoken(&self) -> Vec<String> {
        self.take()
            .into_iter()
            .filter_map(|call| call.strip_prefix("speak: ").map(str::to_string))
            .collect()
    }
}

impl LocalVoiceEngine for FakeEngine {
    fn speak(&self, text: &str) {
        self.record(format!("speak: {text}"));
    }

    fn stop_speaking(&self) {
        self.record("stop speaking".to_string());
    }

    fn set_muted(&self, muted: bool) {
        self.record(format!("muted: {muted}"));
    }

    fn shutdown(&self) -> BoxFuture<'static, ()> {
        self.record("shutdown".to_string());
        Box::pin(async {})
    }
}

struct Harness {
    chat: ChatWidget,
    events: UnboundedReceiver<AppEvent>,
    ops: UnboundedReceiver<AppCommand>,
    engine: Arc<FakeEngine>,
    voice_events: mpsc::Sender<LocalVoiceEvent>,
    thread_id: codex_protocol::ThreadId,
}

impl Harness {
    async fn start() -> Self {
        Self::start_on(ThreadId::new()).await
    }

    async fn start_on(thread_id: ThreadId) -> Self {
        let (mut chat, _sender, events, ops) = make_chatwidget_manual_with_sender().await;
        let engine = Arc::new(FakeEngine::default());
        let (voice_events, receiver) = mpsc::channel(16);
        let receiver = Rc::new(RefCell::new(Some(receiver)));
        let fake = Arc::clone(&engine);
        set_local_voice_starter_for_test(Some(move |_config: LocalVoiceConfig| {
            (
                Arc::clone(&fake) as Arc<dyn LocalVoiceEngine>,
                receiver
                    .borrow_mut()
                    .take()
                    .expect("one engine per harness"),
            )
        }));
        // Local voice never asks for the packaged voice host.
        set_realtime_voice_supported_for_test(Some(false));
        chat.set_realtime_backend_for_test(RealtimeBackend::Local);
        chat.thread_id = Some(thread_id);
        chat.toggle_realtime_conversation();
        assert!(chat.local_voice_running(), "local voice should start");
        assert_eq!(
            chat.realtime_conversation.phase,
            RealtimeConversationPhase::Active
        );
        let mut harness = Self {
            chat,
            events,
            ops,
            engine,
            voice_events,
            thread_id,
        };
        harness.drain();
        harness
    }

    fn event(&mut self, event: LocalVoiceEvent) {
        let attempt_id = self.chat.local_voice_attempt_for_test();
        self.chat.on_local_voice_event(attempt_id, event);
    }

    fn say(&mut self, text: &str) {
        self.event(LocalVoiceEvent::SpeechStarted);
        self.event(LocalVoiceEvent::SpeechEnded);
        self.event(LocalVoiceEvent::Transcript {
            text: text.to_string(),
        });
    }

    /// Ops and app events since the last drain, plus rendered history text.
    fn drain(&mut self) -> Drained {
        let mut drained = Drained::default();
        while let Ok(op) = self.ops.try_recv() {
            drained.ops.push(op);
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                AppEvent::CodexOp(op) | AppEvent::SubmitThreadOp { op, .. } => drained.ops.push(op),
                AppEvent::InsertHistoryCell(cell) => drained.history.extend(
                    cell.display_lines(/*width*/ 200)
                        .into_iter()
                        .map(|line| line.to_string()),
                ),
                event => drained.events.push(event),
            }
        }
        drained
    }

    fn user_item(&mut self, turn_id: &str, text: &str) {
        self.chat.handle_server_notification(
            ServerNotification::ItemStarted(ItemStartedNotification::new(
                self.thread_id.to_string(),
                turn_id.to_string(),
                ThreadItemPayload::UserMessage {
                    id: format!("user-{turn_id}"),
                    client_id: None,
                    content: vec![UserInput::Text {
                        text: text.to_string(),
                        text_elements: Vec::new(),
                    }],
                },
                /*started_at_ms*/ 0,
            )),
            /*replay_kind*/ None,
        );
    }

    fn complete_turn(&mut self, turn_id: &str, answer: &str) {
        self.chat.handle_server_notification(
            ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: self.thread_id.to_string(),
                turn: Turn {
                    id: turn_id.to_string(),
                    root_turn_id: None,
                    items: vec![ThreadItem::untimed(ThreadItemPayload::AgentMessage {
                        moderation: None,
                        id: format!("agent-{turn_id}"),
                        text: answer.to_string(),
                        phase: Some(MessagePhase::FinalAnswer),
                        questions: None,
                        memory_citation: None,
                        delivery: None,
                    })],
                    items_view: TurnItemsView::Summary,
                    status: TurnStatus::Completed,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                },
            }),
            /*replay_kind*/ None,
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        set_local_voice_starter_for_test(None::<fn(LocalVoiceConfig) -> LocalVoiceStart>);
        set_realtime_voice_supported_for_test(None);
    }
}

#[derive(Default)]
struct Drained {
    ops: Vec<AppCommand>,
    events: Vec<AppEvent>,
    history: Vec<String>,
}

impl Drained {
    fn user_turns(&self) -> Vec<String> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                AppCommand::UserTurn { items, .. } => Some(
                    items
                        .iter()
                        .filter_map(|item| match item {
                            UserInput::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => None,
            })
            .collect()
    }

    fn realtime_speech(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| matches!(op, AppCommand::RealtimeConversationSpeech { .. }))
            .count()
    }

    fn history_text(&self) -> String {
        self.history.join("\n")
    }
}

fn exec_approval(thread_id: ThreadId, id: &str, command: &str) -> ApprovalRequest {
    ApprovalRequest::Exec(ExecApprovalRequest {
        kind: Default::default(),
        thread_id,
        thread_label: None,
        id: id.to_string(),
        environment_id: None,
        command: vec!["bash".to_string(), "-lc".to_string(), command.to_string()],
        reason: None,
        available_decisions: vec![
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::AcceptForSession,
            CommandExecutionApprovalDecision::Decline,
            CommandExecutionApprovalDecision::Cancel,
        ],
        network_approval_context: None,
        additional_permissions: None,
    })
}

fn exec_decisions(drained: &Drained) -> Vec<(String, CommandExecutionApprovalDecision)> {
    drained
        .ops
        .iter()
        .filter_map(|op| match op {
            AppCommand::ExecApproval { id, decision, .. } => Some((id.clone(), decision.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn transcript_submits_a_user_turn_like_the_composer() {
    let mut h = Harness::start().await;

    h.say("add a test for the parser");

    let drained = h.drain();
    assert_eq!(drained.user_turns(), vec!["add a test for the parser"]);
    assert_eq!(drained.realtime_speech(), 0);
    assert_eq!(h.engine.take_spoken(), Vec::<String>::new());
    assert_eq!(h.chat.local_voice_activity_for_test(), Some("listening"));
}

#[tokio::test]
async fn transcript_while_typing_keeps_the_composer_draft() {
    let mut h = Harness::start().await;
    h.chat.apply_external_edit("half typed".to_string());

    h.say("run the tests");

    assert_eq!(h.drain().user_turns(), vec!["run the tests"]);
    assert_eq!(h.chat.composer_text_with_pending(), "half typed");
}

#[tokio::test]
async fn voice_turn_answer_is_spoken_short_without_code() {
    let mut h = Harness::start().await;
    h.say("add a test for the parser");
    h.drain();
    h.user_item("turn-1", "add a test for the parser");

    h.complete_turn(
        "turn-1",
        "I added `parser_handles_empty_input` to **parser_tests.rs**.\n\n\
         ```rust\n#[test]\nfn parser_handles_empty_input() {}\n```\n\n\
         All 12 tests pass. I also cleaned up the imports. Then I ran clippy.",
    );

    assert_eq!(
        h.engine.take_spoken(),
        vec![
            "I added parser_handles_empty_input to parser_tests.rs. All 12 tests pass. \
             Details are on screen."
                .to_string()
        ]
    );
    assert_eq!(h.drain().realtime_speech(), 0);
}

/// A Claude Code session: the backend (driving a fake `claude`, python3 -I)
/// produces the turn's notifications, and the spoken answer comes from the
/// final agent message its `turn/completed` carries.
#[tokio::test]
async fn claude_code_voice_turn_answer_is_spoken() {
    use codex_app_server_client::AppServerEvent;
    use codex_app_server_protocol::ClientRequest;
    use codex_app_server_protocol::RequestId;
    use codex_app_server_protocol::ThreadStartResponse;
    use codex_app_server_protocol::TurnStartParams;
    use codex_app_server_protocol::TurnStartResponse;

    let dir = tempfile::tempdir().expect("tempdir");
    let Some(script) = codex_claude_agent_backend::test_support::write_fake_claude(dir.path())
    else {
        eprintln!("python3 not found; skipping");
        return;
    };
    let mut options = codex_claude_agent_backend::ClaudeCodeOptions::new(dir.path().to_path_buf());
    options.claude_bin = script;
    options.env = vec![
        (
            "FAKE_CLAUDE_LOG".into(),
            dir.path().join("log.jsonl").into(),
        ),
        ("FAKE_CLAUDE_SCENARIO".into(), "text".into()),
    ];
    let mut client = codex_claude_agent_backend::start(options);
    let started: ThreadStartResponse = client
        .request_typed(ClientRequest::ThreadStart {
            request_id: RequestId::Integer(1),
            params: Default::default(),
        })
        .await
        .expect("thread/start");
    let thread_id = ThreadId::from_string(&started.thread.id).expect("thread id");
    let mut h = Harness::start_on(thread_id).await;

    let spoken_request = "list the files in this folder";
    h.say(spoken_request);
    assert_eq!(h.drain().user_turns(), vec![spoken_request]);
    // The App submits that user turn to the backend as turn/start.
    let _: TurnStartResponse = client
        .request_typed(ClientRequest::TurnStart {
            request_id: RequestId::Integer(2),
            params: TurnStartParams {
                thread_id: started.thread.id.clone(),
                input: vec![UserInput::Text {
                    text: spoken_request.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await
        .expect("turn/start");
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), client.next_event())
            .await
            .expect("an event within 10s")
            .expect("backend open");
        let AppServerEvent::ServerNotification(notification) = event else {
            continue;
        };
        let done = matches!(*notification, ServerNotification::TurnCompleted(_));
        h.chat
            .handle_server_notification(*notification, /*replay_kind*/ None);
        if done {
            break;
        }
    }

    assert_eq!(
        h.engine.take_spoken(),
        vec!["Hello from Claude.".to_string()]
    );
    assert!(
        h.drain().history_text().contains("Hello from Claude."),
        "the answer is also on screen"
    );
    client.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn typed_turns_are_not_spoken() {
    let mut h = Harness::start().await;
    h.chat.apply_external_edit("typed question".to_string());
    h.chat.handle_key_event(crossterm::event::KeyEvent::from(
        crossterm::event::KeyCode::Enter,
    ));
    assert_eq!(h.drain().user_turns(), vec!["typed question"]);
    h.user_item("turn-typed", "typed question");

    h.complete_turn("turn-typed", "Here is the answer.");

    assert_eq!(h.engine.take_spoken(), Vec::<String>::new());

    // A voice turn after it is still spoken.
    h.say("and now by voice");
    h.user_item("turn-voice", "and now by voice");
    h.complete_turn("turn-voice", "Spoken answer.");
    assert_eq!(h.engine.take_spoken(), vec!["Spoken answer.".to_string()]);
}

#[tokio::test]
async fn spoken_approval_needs_confirm_and_never_becomes_a_turn() {
    let mut h = Harness::start().await;
    let thread_id = h.thread_id;
    h.chat
        .push_approval_request(exec_approval(thread_id, "exec-1", "cargo test"));
    assert_eq!(
        h.engine.take_spoken(),
        vec!["Codex wants to run cargo test. Say approve or deny.".to_string()]
    );

    h.say("approve");
    let drained = h.drain();
    assert_eq!(exec_decisions(&drained), Vec::new());
    assert_eq!(drained.user_turns(), Vec::<String>::new());
    assert!(drained.history_text().contains("approve"));
    assert_eq!(
        h.engine.take_spoken(),
        vec!["Approve cargo test? Say confirm.".to_string()]
    );

    h.say("confirm");
    let drained = h.drain();
    assert_eq!(
        exec_decisions(&drained),
        vec![(
            "exec-1".to_string(),
            CommandExecutionApprovalDecision::Accept
        )]
    );
    assert_eq!(drained.user_turns(), Vec::<String>::new());
    assert_eq!(drained.realtime_speech(), 0);
}

#[tokio::test]
async fn spoken_denial_applies_at_once() {
    let mut h = Harness::start().await;
    let thread_id = h.thread_id;
    h.chat
        .push_approval_request(exec_approval(thread_id, "exec-2", "rm -rf build"));
    h.engine.take();

    h.say("no");

    let drained = h.drain();
    assert_eq!(
        exec_decisions(&drained),
        vec![(
            "exec-2".to_string(),
            CommandExecutionApprovalDecision::Decline
        )]
    );
    assert_eq!(drained.user_turns(), Vec::<String>::new());
    assert_eq!(h.engine.take_spoken(), vec!["Denied.".to_string()]);
}

#[tokio::test]
async fn other_speech_during_an_approval_is_not_sent() {
    let mut h = Harness::start().await;
    let thread_id = h.thread_id;
    h.chat
        .push_approval_request(exec_approval(thread_id, "exec-3", "cargo build"));
    h.engine.take();

    h.say("what does that command do");

    let drained = h.drain();
    assert_eq!(drained.user_turns(), Vec::<String>::new());
    assert_eq!(exec_decisions(&drained), Vec::new());
    assert_eq!(
        h.engine.take_spoken(),
        vec!["Say approve or deny.".to_string()]
    );
}

#[tokio::test]
async fn voice_command_goes_to_the_orchestrator_not_the_model() {
    let mut h = Harness::start().await;

    h.say("what's running");

    let drained = h.drain();
    assert_eq!(drained.user_turns(), Vec::<String>::new());
    assert!(
        drained.events.iter().any(|event| matches!(
            event,
            AppEvent::VoiceCommand { transcript, .. } if transcript == "what's running"
        )),
        "expected a voice command, got {:?}",
        drained.events
    );

    // The orchestrator's reply is spoken locally.
    h.chat
        .speak_voice_command_result("Two agents. Build is running.", "build: running");
    assert_eq!(
        h.engine.take_spoken(),
        vec!["Two agents. Build is running.".to_string()]
    );
    assert_eq!(h.drain().realtime_speech(), 0);
}

#[tokio::test]
async fn turn_errors_are_spoken_locally() {
    let mut h = Harness::start().await;

    h.chat
        .speak_realtime_turn_error("stream disconnected before completion");

    assert_eq!(
        h.engine.take_spoken(),
        vec!["Codex hit an error: stream disconnected before completion.".to_string()]
    );
    assert_eq!(h.drain().realtime_speech(), 0);
}

#[tokio::test]
async fn engine_error_is_shown_once_with_its_fix() {
    let mut h = Harness::start().await;
    let error = LocalVoiceEvent::Error {
        stage: VoiceStage::Capture,
        message: "failed to start `ffmpeg`: No such file or directory (os error 2)".to_string(),
    };

    h.event(error.clone());
    h.event(error);

    let history = h.drain().history_text();
    assert_eq!(
        history.matches("brew install ffmpeg").count(),
        1,
        "{history}"
    );
    assert_eq!(
        h.engine.take_spoken(),
        vec!["I can't use the microphone. Details are on screen.".to_string()]
    );
    assert_eq!(
        h.chat.local_voice_activity_for_test(),
        Some("mic unavailable")
    );
}

#[tokio::test]
async fn approval_prompt_interrupts_a_spoken_answer_but_status_queues() {
    let mut h = Harness::start().await;
    h.say("explain the build");
    h.user_item("turn-1", "explain the build");
    h.complete_turn("turn-1", "The build has three stages.");
    h.event(LocalVoiceEvent::SpeakingStarted);
    h.chat.speak_voice_command_result("No agents.", "");
    assert_eq!(
        h.engine.take(),
        vec![
            "speak: The build has three stages.".to_string(),
            "speak: No agents.".to_string(),
        ]
    );

    let thread_id = h.thread_id;
    h.chat
        .push_approval_request(exec_approval(thread_id, "exec-4", "cargo test"));

    assert_eq!(
        h.engine.take(),
        vec![
            "stop speaking".to_string(),
            "speak: Codex wants to run cargo test. Say approve or deny.".to_string(),
        ]
    );
}

#[tokio::test]
async fn activity_follows_engine_events() {
    let mut h = Harness::start().await;
    h.event(LocalVoiceEvent::SpeechStarted);
    assert_eq!(h.chat.local_voice_activity_for_test(), Some("hearing you"));
    h.event(LocalVoiceEvent::SpeechEnded);
    assert_eq!(h.chat.local_voice_activity_for_test(), Some("transcribing"));
    h.event(LocalVoiceEvent::Listening);
    assert_eq!(h.chat.local_voice_activity_for_test(), Some("listening"));
    h.event(LocalVoiceEvent::SpeakingStarted);
    assert_eq!(h.chat.local_voice_activity_for_test(), Some("speaking"));
    h.event(LocalVoiceEvent::SpeakingFinished);
    assert_eq!(h.chat.local_voice_activity_for_test(), Some("listening"));
}

#[tokio::test]
async fn engine_events_reach_the_app_for_this_attempt_only() {
    let mut h = Harness::start().await;
    h.voice_events
        .send(LocalVoiceEvent::Transcript {
            text: "hello".to_string(),
        })
        .await
        .expect("forwarder is listening");
    let mut forwarded = None;
    for _ in 0..20 {
        tokio::task::yield_now().await;
        if let Some(event) = h.drain().events.into_iter().find_map(|event| match event {
            AppEvent::LocalVoice {
                thread_id,
                attempt_id,
                event,
            } => Some((thread_id, attempt_id, event)),
            _ => None,
        }) {
            forwarded = Some(event);
            break;
        }
    }
    let (thread_id, attempt_id, event) = forwarded.expect("forwarded engine event");
    assert_eq!(thread_id, h.thread_id);
    assert_eq!(attempt_id, h.chat.local_voice_attempt_for_test());

    // A stale attempt's events are ignored.
    h.chat.on_local_voice_event(attempt_id + 1, event.clone());
    assert_eq!(h.drain().user_turns(), Vec::<String>::new());
    h.chat.on_local_voice_event(attempt_id, event);
    assert_eq!(h.drain().user_turns(), vec!["hello"]);
}

#[tokio::test]
async fn mute_and_stop_control_the_engine() {
    let mut h = Harness::start().await;

    h.chat.toggle_realtime_microphone();
    assert_eq!(
        h.engine.take(),
        vec!["muted: true".to_string(), "stop speaking".to_string()]
    );
    assert!(h.chat.realtime_conversation.microphone_muted);
    h.chat.toggle_realtime_microphone();
    assert_eq!(h.engine.take(), vec!["muted: false".to_string()]);

    h.chat.toggle_realtime_conversation();
    assert_eq!(h.engine.take(), vec!["shutdown".to_string()]);
    assert!(!h.chat.local_voice_running());
    assert_eq!(
        h.chat.realtime_conversation.phase,
        RealtimeConversationPhase::Inactive
    );
    let drained = h.drain();
    // Local voice never talks to the realtime service.
    assert!(!drained.ops.iter().any(|op| matches!(
        op,
        AppCommand::RealtimeConversationStop { .. } | AppCommand::RealtimeConversationStart { .. }
    )));
    assert!(drained.history_text().contains("Voice conversation ended."));
}

#[tokio::test]
async fn app_shutdown_waits_for_the_engine() {
    let mut h = Harness::start().await;

    let shutdown = h.chat.take_local_voice_shutdown().expect("running engine");
    shutdown.await;

    assert_eq!(h.engine.take(), vec!["shutdown".to_string()]);
    assert!(h.chat.take_local_voice_shutdown().is_none());
}

#[tokio::test]
async fn dropping_the_widget_shuts_the_engine_down() {
    let h = Harness::start().await;
    let engine = Arc::clone(&h.engine);
    drop(h);
    assert_eq!(engine.take(), vec!["shutdown".to_string()]);
}

#[tokio::test]
async fn auto_start_uses_local_voice_without_the_voice_host() {
    let (mut chat, _sender, _events, _ops) = make_chatwidget_manual_with_sender().await;
    let engine = Arc::new(FakeEngine::default());
    let fake = Arc::clone(&engine);
    set_local_voice_starter_for_test(Some(move |_config: LocalVoiceConfig| {
        let (_tx, rx) = mpsc::channel(1);
        (Arc::clone(&fake) as Arc<dyn LocalVoiceEngine>, rx)
    }));
    set_realtime_voice_supported_for_test(Some(false));
    chat.set_realtime_backend_for_test(RealtimeBackend::Local);
    chat.config.realtime.session_type = codex_config::config_toml::RealtimeWsMode::Transcription;
    chat.set_realtime_auto_start_for_test(true);
    chat.thread_id = Some(ThreadId::new());

    assert!(chat.auto_start_realtime_conversation());
    assert!(chat.local_voice_running());

    set_local_voice_starter_for_test(None::<fn(LocalVoiceConfig) -> LocalVoiceStart>);
    set_realtime_voice_supported_for_test(None);
}

#[tokio::test]
async fn invalid_local_settings_do_not_start_voice() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    chat.set_realtime_backend_for_test(RealtimeBackend::Local);
    let mut local = serde_json::Map::new();
    local.insert(
        "stt".to_string(),
        serde_json::json!({ "backend": "carrier-pigeon" }),
    );
    chat.config.realtime.local = Some(local);
    chat.thread_id = Some(ThreadId::new());

    chat.toggle_realtime_conversation();

    assert!(!chat.local_voice_running());
    let mut history = String::new();
    while let Ok(event) = events.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event {
            for line in cell.display_lines(/*width*/ 200) {
                history.push_str(&line.to_string());
            }
        }
    }
    assert!(history.contains("[realtime.local]"), "{history}");
}

#[test]
fn local_settings_map_onto_the_engine_config() {
    assert_eq!(local_voice_config(None), Ok(LocalVoiceConfig::default()));
    let local = serde_json::json!({
        "capture": { "device": "2" },
        "tts": { "backend": "command", "command": ["say", "-v", "Samantha", "{text}"] },
    });
    let config = local_voice_config(local.as_object()).expect("valid settings");
    assert_eq!(config.capture.device, "2");
    assert_eq!(
        config.tts,
        codex_local_voice::TtsConfig::Command(codex_local_voice::CommandTtsConfig {
            command: vec![
                "say".to_string(),
                "-v".to_string(),
                "Samantha".to_string(),
                "{text}".to_string()
            ],
        })
    );
    assert_eq!(config.vad, LocalVoiceConfig::default().vad);
}

#[test]
fn spoken_answer_strips_markdown_and_code() {
    assert_eq!(spoken_answer("Tests pass."), "Tests pass.");
    assert_eq!(
        spoken_answer("## Summary\n- Fixed the **parser**\n- Added [docs](https://x.y/z)"),
        "Summary. Fixed the parser. Details are on screen."
    );
    assert_eq!(
        spoken_answer("- Fixed it\n- Added [docs](https://x.y/z)"),
        "Fixed it. Added docs"
    );
    assert_eq!(
        spoken_answer("```sh\ncargo test\n```"),
        "Details are on screen."
    );
    assert_eq!(spoken_answer("   \n"), "");
    assert_eq!(
        spoken_answer("See https://example.com/a for `snake_case_name`."),
        "See a link for snake_case_name."
    );
}

#[test]
fn spoken_answer_keeps_two_sentences_and_forty_words() {
    let long = format!("{}.", vec!["word"; 60].join(" "));
    let spoken = spoken_answer(&long);
    assert!(spoken.ends_with(" Details are on screen."), "{spoken}");
    assert_eq!(
        spoken
            .trim_end_matches(" Details are on screen.")
            .split_whitespace()
            .count(),
        MAX_SPOKEN_ANSWER_WORDS
    );
    assert_eq!(
        spoken_answer("One. Two! Three?"),
        "One. Two! Details are on screen."
    );
    assert_eq!(
        spoken_answer("Version 1.2.3 is out. Done."),
        "Version 1.2.3 is out. Done."
    );
}

#[test]
fn error_notices_name_the_fix() {
    let (key, text) = local_voice_error_notice(
        VoiceStage::Stt,
        "failed to start `mlx_whisper`: No such file or directory (os error 2)",
    );
    assert_eq!(key, "missing:mlx_whisper");
    assert!(text.contains("uv tool install mlx-whisper"), "{text}");

    let (key, text) = local_voice_error_notice(
        VoiceStage::Capture,
        "capture stopped: `ffmpeg` exited with status 1: [avfoundation] Failed to create AV capture input device: Cannot use MacBook Microphone",
    );
    assert_eq!(key, "microphone-access");
    assert!(
        text.contains("System Settings → Privacy & Security → Microphone"),
        "{text}"
    );

    let (key, text) = local_voice_error_notice(VoiceStage::Tts, "player exited with status 1");
    assert_eq!(key, "stage:text-to-speech");
    assert!(text.contains("[realtime.local.tts]"), "{text}");
}

#[tokio::test]
async fn clean_capture_end_is_a_notice_not_an_error() {
    let mut h = Harness::start().await;
    h.event(LocalVoiceEvent::CaptureEnded {
        message: "capture ended: `ffmpeg` exited cleanly".to_string(),
    });

    let history = h.drain().history_text();
    assert!(history.contains("Voice input ended"), "{history}");
    assert!(!history.contains("microphone access"), "{history}");
    assert_eq!(h.engine.take_spoken(), Vec::<String>::new());
    assert_eq!(h.chat.local_voice_activity_for_test(), Some("mic off"));
}
