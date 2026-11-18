//! Spoken orchestration commands: routing, confirmation, choices, approval
//! precedence, and the voice model's handoff copies.
//!
//! The app's side is played by a real `codex_orchestrator::Orchestrator` over
//! in-memory agents, fed exactly as `App` feeds it, so the widget's routing is
//! checked against the engine it mirrors.

use super::voice_approvals::ALL_DECISIONS;
use super::voice_approvals::drain_events;
use super::voice_approvals::exec_approval;
use super::voice_approvals::exec_decision;
use super::voice_approvals::hand_off;
use super::voice_approvals::say;
use super::voice_approvals::spoken;
use super::*;
use crate::app_event::VoiceCommandResult;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_orchestrator::ActivityItem;
use codex_orchestrator::AgentBackend;
use codex_orchestrator::AgentInfo;
use codex_orchestrator::AgentReport;
use codex_orchestrator::AgentStatus;
use codex_orchestrator::BackendError;
use codex_orchestrator::BackendFuture;
use codex_orchestrator::InterruptOutcome;
use codex_orchestrator::ItemStatus;
use codex_orchestrator::Orchestrator;
use codex_orchestrator::SteerOutcome;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;

type Actions = Arc<Mutex<Vec<String>>>;

/// One machine's agents in memory. Steers and interrupts are only recorded.
struct Machine {
    name: &'static str,
    agents: Vec<AgentInfo>,
    actions: Actions,
}

impl AgentBackend for Machine {
    fn machine(&self) -> &str {
        self.name
    }

    fn list_agents(&self) -> BackendFuture<'_, Vec<AgentInfo>> {
        futures::future::ready(Ok(self.agents.clone())).boxed()
    }

    fn agent_activity<'a>(&'a self, id: &'a str) -> BackendFuture<'a, AgentReport> {
        let report = self
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .map(|agent| AgentReport {
                agent: agent.clone(),
                recent: vec![ActivityItem::Command {
                    command: "cargo test".to_string(),
                    status: ItemStatus::InProgress,
                    exit_code: None,
                    output: None,
                }],
            })
            .ok_or_else(|| BackendError::AgentNotFound(id.to_string()));
        futures::future::ready(report).boxed()
    }

    fn steer<'a>(&'a self, id: &'a str, message: &'a str) -> BackendFuture<'a, SteerOutcome> {
        self.actions
            .lock()
            .expect("actions")
            .push(format!("steer {id}: {message}"));
        futures::future::ready(Ok(SteerOutcome::Steered {
            turn_id: "turn-1".to_string(),
        }))
        .boxed()
    }

    fn interrupt<'a>(&'a self, id: &'a str) -> BackendFuture<'a, InterruptOutcome> {
        self.actions
            .lock()
            .expect("actions")
            .push(format!("interrupt {id}"));
        futures::future::ready(Ok(InterruptOutcome::Interrupted {
            turn_id: "turn-1".to_string(),
        }))
        .boxed()
    }
}

fn running(id: &str, name: &str) -> AgentInfo {
    AgentInfo {
        id: id.to_string(),
        name: name.to_string(),
        status: AgentStatus::Running,
        cwd: None,
        preview: String::new(),
        updated_at: None,
        turn_started_at: None,
        latest: None,
    }
}

/// build and lint on desktop; build and tests on laptop.
fn engine() -> (Orchestrator, Actions) {
    let actions = Actions::default();
    let desktop = Machine {
        name: "desktop",
        agents: vec![running("t-build-d", "build"), running("t-lint", "lint")],
        actions: Arc::clone(&actions),
    };
    let laptop = Machine {
        name: "laptop",
        agents: vec![running("t-build-l", "build"), running("t-tests", "tests")],
        actions: Arc::clone(&actions),
    };
    let engine = Orchestrator::new(vec![Box::new(desktop), Box::new(laptop)])
        .with_known_machines(vec!["studio".to_string()]);
    (engine, actions)
}

fn actions(actions: &Actions) -> Vec<String> {
    actions.lock().expect("actions").clone()
}

/// Activate voice and give the widget the engine's context, as the app does
/// when voice starts.
async fn voice_with_engine(chat: &mut ChatWidget) -> (ThreadId, Orchestrator, Actions) {
    let thread_id = activate_voice(chat);
    let (mut engine, log) = engine();
    engine.snapshot().await;
    chat.on_voice_command_result(&VoiceCommandResult {
        seq: 0,
        outcome: None,
        spoken_override: None,
        context: engine.voice_context(),
        last_agent: None,
    });
    (thread_id, engine, log)
}

/// Play the app: run every voice-command request through the engine and hand
/// each answer back to the widget. Returns the transcripts that were run.
async fn answer(
    chat: &mut ChatWidget,
    events: &mut UnboundedReceiver<AppEvent>,
    engine: &mut Orchestrator,
) -> Vec<String> {
    let mut ran = Vec::new();
    let pending: Vec<AppEvent> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    for event in pending {
        let (seq, outcome) = match event {
            AppEvent::VoiceCommand {
                seq,
                transcript,
                approval_prompt_open,
                ..
            } => {
                ran.push(transcript.clone());
                let outcome = engine
                    .handle_utterance(&transcript, approval_prompt_open)
                    .await;
                (seq, Some(outcome))
            }
            AppEvent::VoiceCommandCancel { seq, .. } => {
                engine.cancel();
                (seq, None)
            }
            AppEvent::VoiceCommandRefresh { seq, .. } => {
                engine.snapshot().await;
                (seq, None)
            }
            other => {
                chat.app_event_tx.send(other);
                continue;
            }
        };
        chat.on_voice_command_result(&VoiceCommandResult {
            seq,
            outcome,
            spoken_override: None,
            context: engine.voice_context(),
            last_agent: engine.last_agent().cloned(),
        });
    }
    ran
}

/// Voice-command requests waiting for the app, without answering them.
fn voice_requests(events: &mut UnboundedReceiver<AppEvent>) -> Vec<String> {
    let pending: Vec<AppEvent> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    pending
        .into_iter()
        .filter_map(|event| match event {
            AppEvent::VoiceCommand { transcript, .. } => Some(transcript),
            AppEvent::VoiceCommandCancel { .. } => Some("<cancel>".to_string()),
            _ => None,
        })
        .collect()
}

fn delegated_may_speak(chat: &ChatWidget, turn_id: &str) -> Option<bool> {
    match chat.realtime_conversation.turn_origins.get(turn_id) {
        Some(super::super::RealtimeTurnOrigin::Delegated { may_speak, .. }) => Some(*may_speak),
        _ => None,
    }
}

#[tokio::test]
async fn a_command_is_sent_to_the_app_and_kept_from_the_model() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (thread_id, mut engine, log) = voice_with_engine(&mut chat).await;

    say(&mut chat, "What's running?");
    let generation = chat.realtime_conversation.input_generation;
    // The voice model's handoff of the same words never becomes a model request.
    hand_off(&mut chat, thread_id, "handoff-list", "What's running?");
    assert_eq!(delegated_may_speak(&chat, "handoff-list"), Some(false));
    assert!(chat.should_hide_realtime_delegation("handoff-list"));
    assert_eq!(chat.realtime_conversation.input_generation, generation);

    assert_eq!(
        answer(&mut chat, &mut events, &mut engine).await,
        vec!["What's running?".to_string()]
    );
    let speech = spoken(&mut ops);
    assert_eq!(speech.len(), 1, "{speech:?}");
    assert!(speech[0].starts_with("Four agents."), "{speech:?}");
    let (submitted, history) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "{submitted:?}");
    assert!(history.contains("build"), "{history}");
    assert_eq!(actions(&log), Vec::<String>::new());
}

#[tokio::test]
async fn ordinary_speech_still_reaches_the_model() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (thread_id, _engine, _log) = voice_with_engine(&mut chat).await;

    for (index, utterance) in [
        "tell me how this function works",
        "ask the user to confirm before deleting",
        "stop using unwrap in parser.rs",
        "stop",
        "switch to the main branch",
        "connect to the database",
        "what is this function doing",
        "what's running on port 3000",
        "don't stop build",
        "please run the tests on build",
    ]
    .into_iter()
    .enumerate()
    {
        say(&mut chat, utterance);
        assert_eq!(
            voice_requests(&mut events),
            Vec::<String>::new(),
            "{utterance}"
        );
        assert_eq!(spoken(&mut ops), Vec::<String>::new(), "{utterance}");
        let turn_id = format!("handoff-{index}");
        hand_off(&mut chat, thread_id, &turn_id, utterance);
        assert_eq!(
            delegated_may_speak(&chat, &turn_id),
            Some(true),
            "{utterance} must reach the model"
        );
    }
}

#[tokio::test]
async fn voice_commands_can_be_turned_off() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (thread_id, _engine, _log) = voice_with_engine(&mut chat).await;
    chat.config.realtime.voice_commands = false;

    say(&mut chat, "what's running");
    hand_off(&mut chat, thread_id, "handoff-list", "what's running");

    assert_eq!(voice_requests(&mut events), Vec::<String>::new());
    assert_eq!(spoken(&mut ops), Vec::<String>::new());
    assert_eq!(delegated_may_speak(&chat, "handoff-list"), Some(true));
}

#[tokio::test]
async fn approvals_win_over_stop_it_while_a_prompt_is_open() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (thread_id, mut engine, log) = voice_with_engine(&mut chat).await;
    // "it" now means lint.
    say(&mut chat, "what's lint doing");
    answer(&mut chat, &mut events, &mut engine).await;
    spoken(&mut ops);

    chat.push_approval_request(exec_approval(thread_id, "exec-1", "ls", &ALL_DECISIONS));
    spoken(&mut ops);
    say(&mut chat, "stop it");

    assert_eq!(
        answer(&mut chat, &mut events, &mut engine).await,
        Vec::<String>::new()
    );
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert_eq!(
        submitted
            .iter()
            .filter_map(exec_decision)
            .collect::<Vec<_>>(),
        vec![("exec-1", &CommandExecutionApprovalDecision::Cancel)]
    );
    assert_eq!(spoken(&mut ops), vec!["Stopped.".to_string()]);
    assert_eq!(actions(&log), Vec::<String>::new());
    assert!(
        chat.realtime_conversation
            .voice_controls
            .voice_commands
            .pending
            .is_none()
    );
}

#[tokio::test]
async fn a_named_command_still_works_while_an_approval_prompt_is_open() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (thread_id, mut engine, log) = voice_with_engine(&mut chat).await;
    chat.push_approval_request(exec_approval(thread_id, "exec-1", "ls", &ALL_DECISIONS));
    spoken(&mut ops);

    say(&mut chat, "stop tests");
    answer(&mut chat, &mut events, &mut engine).await;
    assert_eq!(
        spoken(&mut ops),
        vec!["Stop tests on laptop? Say confirm.".to_string()]
    );
    // "confirm" belongs to the open approval prompt, which has no approve
    // pending, so it neither approves nor stops the agent.
    say(&mut chat, "confirm");
    assert_eq!(
        answer(&mut chat, &mut events, &mut engine).await,
        Vec::<String>::new()
    );
    assert_eq!(
        spoken(&mut ops),
        vec!["Say approve first, then confirm.".to_string()]
    );
    assert_eq!(actions(&log), Vec::<String>::new());
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "{submitted:?}");
}

#[tokio::test]
async fn a_steer_runs_only_after_confirm() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (_thread_id, mut engine, log) = voice_with_engine(&mut chat).await;

    say(&mut chat, "tell lint to run the tests");
    answer(&mut chat, &mut events, &mut engine).await;
    assert_eq!(
        spoken(&mut ops),
        vec!["Tell lint on desktop: run the tests. Say confirm.".to_string()]
    );
    assert_eq!(actions(&log), Vec::<String>::new());

    say(&mut chat, "Confirm.");
    assert_eq!(
        answer(&mut chat, &mut events, &mut engine).await,
        vec!["Confirm.".to_string()]
    );
    assert_eq!(
        spoken(&mut ops),
        vec!["Sent to lint on desktop.".to_string()]
    );
    assert_eq!(
        actions(&log),
        vec!["steer t-lint: run the tests".to_string()]
    );
    assert!(
        chat.realtime_conversation
            .voice_controls
            .voice_commands
            .pending
            .is_none()
    );
}

#[tokio::test]
async fn anything_but_confirm_cancels_a_steer() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (thread_id, mut engine, log) = voice_with_engine(&mut chat).await;

    // Ordinary speech cancels and still reaches the model.
    say(&mut chat, "tell lint to run the tests");
    answer(&mut chat, &mut events, &mut engine).await;
    spoken(&mut ops);
    say(&mut chat, "what time is it");
    hand_off(&mut chat, thread_id, "handoff-time", "what time is it");
    assert_eq!(delegated_may_speak(&chat, "handoff-time"), Some(true));
    assert_eq!(
        answer(&mut chat, &mut events, &mut engine).await,
        Vec::<String>::new()
    );
    assert_eq!(spoken(&mut ops), vec!["Cancelled.".to_string()]);
    assert!(engine.pending().is_none());

    // A refusal cancels and is kept from the model.
    say(&mut chat, "tell lint to run the tests");
    answer(&mut chat, &mut events, &mut engine).await;
    spoken(&mut ops);
    say(&mut chat, "never mind");
    assert_eq!(
        answer(&mut chat, &mut events, &mut engine).await,
        vec!["never mind".to_string()]
    );
    assert_eq!(spoken(&mut ops), vec!["Cancelled.".to_string()]);

    // A late "confirm" has nothing to run.
    say(&mut chat, "confirm");
    assert_eq!(
        answer(&mut chat, &mut events, &mut engine).await,
        Vec::<String>::new()
    );
    assert_eq!(actions(&log), Vec::<String>::new());
}

#[tokio::test]
async fn an_ambiguous_name_asks_which_one_then_takes_the_answer() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (thread_id, mut engine, _log) = voice_with_engine(&mut chat).await;

    say(&mut chat, "what's build doing");
    answer(&mut chat, &mut events, &mut engine).await;
    assert_eq!(
        spoken(&mut ops),
        vec!["Which agent: build on desktop or build on laptop?".to_string()]
    );

    say(&mut chat, "the one on laptop");
    hand_off(&mut chat, thread_id, "handoff-choice", "the one on laptop");
    assert_eq!(delegated_may_speak(&chat, "handoff-choice"), Some(false));
    assert_eq!(
        answer(&mut chat, &mut events, &mut engine).await,
        vec!["the one on laptop".to_string()]
    );
    let speech = spoken(&mut ops);
    assert_eq!(speech.len(), 1, "{speech:?}");
    assert!(
        speech[0].to_lowercase().starts_with("build on laptop"),
        "{speech:?}"
    );
    assert!(
        chat.realtime_conversation
            .voice_controls
            .voice_commands
            .pending
            .is_none()
    );

    // "it" now means build on laptop.
    say(&mut chat, "stop it");
    answer(&mut chat, &mut events, &mut engine).await;
    assert_eq!(
        spoken(&mut ops),
        vec!["Stop build on laptop? Say confirm.".to_string()]
    );
}

#[tokio::test]
async fn handoff_copies_are_swallowed_and_never_run_in_either_order() {
    for handoff_first in [true, false] {
        let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
        let (thread_id, mut engine, log) = voice_with_engine(&mut chat).await;
        let steer = "tell lint to run the tests";

        if handoff_first {
            hand_off(&mut chat, thread_id, "handoff-steer", steer);
            assert_eq!(
                voice_requests(&mut events),
                Vec::<String>::new(),
                "a handoff never runs a command"
            );
            say(&mut chat, steer);
        } else {
            say(&mut chat, steer);
            hand_off(&mut chat, thread_id, "handoff-steer", steer);
        }
        assert_eq!(
            answer(&mut chat, &mut events, &mut engine).await,
            vec![steer.to_string()],
            "handoff_first={handoff_first}"
        );
        assert_eq!(delegated_may_speak(&chat, "handoff-steer"), Some(false));
        assert!(chat.should_hide_realtime_delegation("handoff-steer"));

        // The confirmation, too, in either order.
        if handoff_first {
            hand_off(&mut chat, thread_id, "handoff-confirm", "confirm");
            assert_eq!(
                answer(&mut chat, &mut events, &mut engine).await,
                Vec::<String>::new()
            );
            assert_eq!(actions(&log), Vec::<String>::new());
            say(&mut chat, "confirm");
        } else {
            say(&mut chat, "confirm");
            hand_off(&mut chat, thread_id, "handoff-confirm", "confirm");
        }
        answer(&mut chat, &mut events, &mut engine).await;
        assert!(chat.should_hide_realtime_delegation("handoff-confirm"));
        assert_eq!(
            actions(&log),
            vec!["steer t-lint: run the tests".to_string()],
            "handoff_first={handoff_first}"
        );
        let speech = spoken(&mut ops);
        assert_eq!(
            speech.last().map(String::as_str),
            Some("Sent to lint on desktop."),
            "{speech:?}"
        );
    }
}

#[tokio::test]
async fn a_handed_off_confirm_never_runs_a_pending_command() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (thread_id, mut engine, log) = voice_with_engine(&mut chat).await;
    say(&mut chat, "stop tests");
    answer(&mut chat, &mut events, &mut engine).await;
    assert_eq!(
        spoken(&mut ops),
        vec!["Stop tests on laptop? Say confirm.".to_string()]
    );

    // The model answers on the user's behalf; the user never did.
    for (turn_id, text) in [
        ("handoff-confirm", "confirm"),
        ("handoff-yes", "yes"),
        ("handoff-no", "no"),
    ] {
        hand_off(&mut chat, thread_id, turn_id, text);
        assert_eq!(delegated_may_speak(&chat, turn_id), Some(false), "{text}");
        assert!(chat.should_hide_realtime_delegation(turn_id), "{text}");
    }
    assert_eq!(
        answer(&mut chat, &mut events, &mut engine).await,
        Vec::<String>::new()
    );
    assert_eq!(actions(&log), Vec::<String>::new());
    assert_eq!(spoken(&mut ops), Vec::<String>::new());
    assert!(
        engine.pending().is_some(),
        "the stop still waits for the user"
    );

    say(&mut chat, "confirm");
    answer(&mut chat, &mut events, &mut engine).await;
    assert_eq!(actions(&log), vec!["interrupt t-tests".to_string()]);
    assert_eq!(
        spoken(&mut ops),
        vec!["Stopped tests on laptop.".to_string()]
    );
}

#[tokio::test]
async fn a_stale_answer_does_not_restore_a_cancelled_confirmation() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (_thread_id, mut engine, log) = voice_with_engine(&mut chat).await;

    // The user moves on before the app answers the steer.
    say(&mut chat, "tell lint to run the tests");
    say(&mut chat, "what time is it");
    assert_eq!(
        answer(&mut chat, &mut events, &mut engine).await,
        vec!["tell lint to run the tests".to_string()]
    );
    assert!(
        chat.realtime_conversation
            .voice_controls
            .voice_commands
            .pending
            .is_none()
    );
    assert!(engine.pending().is_none());
    spoken(&mut ops);

    say(&mut chat, "confirm");
    answer(&mut chat, &mut events, &mut engine).await;
    assert_eq!(actions(&log), Vec::<String>::new());
}

#[tokio::test]
async fn pending_commands_reset_with_the_call() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let (_thread_id, mut engine, _log) = voice_with_engine(&mut chat).await;
    say(&mut chat, "stop tests");
    answer(&mut chat, &mut events, &mut engine).await;
    spoken(&mut ops);
    assert!(
        chat.realtime_conversation
            .voice_controls
            .voice_commands
            .pending
            .is_some()
    );

    chat.reset_realtime_conversation();

    assert!(
        chat.realtime_conversation
            .voice_controls
            .voice_commands
            .pending
            .is_none()
    );
}
