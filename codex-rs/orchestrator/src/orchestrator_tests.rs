use crate::ActivityItem;
use crate::AgentBackend;
use crate::AgentInfo;
use crate::AgentStatus;
use crate::ItemStatus;
use crate::MachineError;
use crate::MachineSnapshot;
use crate::Orchestrator;
use crate::Outcome;
use crate::OutcomeStatus;
use crate::PendingAction;
use crate::Snapshot;
use crate::test_support::Behavior;
use crate::test_support::Call;
use crate::test_support::FakeBackend;
use crate::test_support::FakeState;
use crate::test_support::agent;
use codex_voice_commands::AgentRef;
use codex_voice_commands::MAX_SPOKEN_WORDS;
use codex_voice_commands::VoiceCommand;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;

const NOW: i64 = 1_000_000;
const TIMEOUT: Duration = Duration::from_millis(100);

fn running(id: &str, name: &str, command: &str) -> AgentInfo {
    AgentInfo {
        turn_started_at: Some(NOW - 125),
        latest: Some(ActivityItem::Command {
            command: command.to_string(),
            status: ItemStatus::InProgress,
            exit_code: None,
            output: None,
        }),
        ..agent(id, name, AgentStatus::Running)
    }
}

fn agent_ref(id: &str, name: &str, machine: &str) -> AgentRef {
    AgentRef {
        id: id.to_string(),
        name: name.to_string(),
        machine: Some(machine.to_string()),
    }
}

fn orchestrator(backends: Vec<FakeBackend>) -> Orchestrator {
    let backends: Vec<Box<dyn AgentBackend>> = backends
        .into_iter()
        .map(|backend| Box::new(backend) as Box<dyn AgentBackend>)
        .collect();
    Orchestrator::new(backends)
        .with_timeout(TIMEOUT)
        .with_clock(|| NOW)
}

/// build (running) on desktop, tests (waiting for approval) and docs (idle)
/// on laptop. Returns the laptop's state for call assertions.
fn two_machines() -> (Orchestrator, Arc<Mutex<FakeState>>, Arc<Mutex<FakeState>>) {
    let desktop = FakeBackend::new(
        "desktop",
        vec![running("t-build", "build", "cargo build --release")],
    );
    let laptop = FakeBackend::new(
        "laptop",
        vec![
            AgentInfo {
                latest: Some(ActivityItem::Command {
                    command: "/bin/zsh -lc 'cargo test -p codex-core'".to_string(),
                    status: ItemStatus::InProgress,
                    exit_code: None,
                    output: None,
                }),
                ..agent("t-tests", "tests", AgentStatus::WaitingForApproval)
            },
            agent("t-docs", "docs", AgentStatus::Idle),
        ],
    );
    let desktop_state = Arc::clone(&desktop.state);
    let laptop_state = Arc::clone(&laptop.state);
    (
        orchestrator(vec![desktop, laptop]),
        desktop_state,
        laptop_state,
    )
}

fn calls(state: &Arc<Mutex<FakeState>>) -> Vec<Call> {
    state.lock().unwrap().calls.clone()
}

fn assert_spoken_limit(outcome: &Outcome) {
    let words = outcome.spoken.split_whitespace().count();
    assert!(
        words <= MAX_SPOKEN_WORDS,
        "{words} words: {}",
        outcome.spoken
    );
}

#[tokio::test]
async fn snapshot_fans_out_and_reports_a_slow_machine() {
    // desktop answers only once laptop has been asked, which proves the calls
    // run concurrently; studio never answers.
    let ready = Arc::new(Notify::new());
    let desktop = FakeBackend::new(
        "desktop",
        vec![agent("t-build", "build", AgentStatus::Idle)],
    )
    .with_behavior(Behavior::WaitFor(Arc::clone(&ready)));
    let laptop = FakeBackend::new("laptop", vec![agent("t-tests", "tests", AgentStatus::Idle)])
        .with_behavior(Behavior::Signal(ready));
    let studio = FakeBackend::new("mac-studio", vec![agent("t-x", "x", AgentStatus::Idle)])
        .with_behavior(Behavior::Hang);
    let mut engine = orchestrator(vec![desktop, laptop, studio]);

    let snapshot = engine.snapshot().await.clone();

    assert_eq!(
        snapshot,
        Snapshot {
            machines: vec![
                MachineSnapshot {
                    machine: "desktop".to_string(),
                    agents: Ok(vec![agent("t-build", "build", AgentStatus::Idle)]),
                },
                MachineSnapshot {
                    machine: "laptop".to_string(),
                    agents: Ok(vec![agent("t-tests", "tests", AgentStatus::Idle)]),
                },
                MachineSnapshot {
                    machine: "mac-studio".to_string(),
                    agents: Err(MachineError::Timeout { after: TIMEOUT }),
                },
            ],
        }
    );
    let context = engine.voice_context();
    assert_eq!(
        context.known_agents,
        vec![
            agent_ref("t-build", "build", "desktop"),
            agent_ref("t-tests", "tests", "laptop"),
        ]
    );
    assert_eq!(
        context.known_machines,
        vec!["desktop", "laptop", "mac-studio"]
    );

    let listed = engine.handle_utterance("what's running", false).await;
    assert_eq!(listed.status, OutcomeStatus::Done);
    assert_eq!(
        listed.spoken,
        "Two agents. Build on desktop: idle. Tests on laptop: idle. Mac studio didn't answer."
    );
    assert_eq!(
        listed.screen,
        "build on desktop: idle\ntests on laptop: idle\nmac-studio: unreachable (timed out after 100ms)"
    );
}

#[tokio::test]
async fn list_reads_one_line_per_agent_with_approvals_first() {
    let (mut engine, _, _) = two_machines();
    let outcome = engine.handle_utterance("list my agents", false).await;
    assert_eq!(
        outcome.spoken,
        "Three agents. Tests on laptop: waiting for your approval to run cargo test codex-core. \
         Build on desktop: running cargo build, 2 minutes. Plus one more."
    );
    assert_spoken_limit(&outcome);
}

#[tokio::test]
async fn ambiguous_names_across_machines_ask_then_resolve() {
    let desktop = FakeBackend::new("desktop", vec![running("t-1", "build", "make")]);
    let laptop = FakeBackend::new("laptop", vec![running("t-2", "build", "ninja")]);
    let mut engine = orchestrator(vec![desktop, laptop]);

    let question = engine.handle_utterance("what's build doing", false).await;
    assert_eq!(question.status, OutcomeStatus::AwaitingChoice);
    assert_eq!(
        question.spoken,
        "Which agent: build on desktop or build on laptop?"
    );

    let answer = engine.handle_utterance("the one on laptop", false).await;
    assert_eq!(
        answer.command,
        VoiceCommand::AgentStatus {
            agent: agent_ref("t-2", "build", "laptop"),
        }
    );
    assert_eq!(answer.spoken, "Build on laptop: running ninja, 2 minutes.");
    assert_eq!(
        engine.last_agent(),
        Some(&agent_ref("t-2", "build", "laptop"))
    );
}

#[tokio::test]
async fn steer_waits_for_confirm() {
    let (mut engine, desktop, _) = two_machines();

    let asked = engine
        .handle_utterance("tell build to run the tests", false)
        .await;
    assert_eq!(asked.status, OutcomeStatus::AwaitingConfirmation);
    assert_eq!(
        asked.spoken,
        "Tell build on desktop: run the tests. Say confirm."
    );
    assert_eq!(
        engine.pending(),
        Some(&PendingAction::Steer {
            agent: agent_ref("t-build", "build", "desktop"),
            message: "run the tests".to_string(),
        })
    );
    assert_eq!(calls(&desktop), Vec::new());

    let done = engine.handle_utterance("confirm", false).await;
    assert_eq!(done.status, OutcomeStatus::Done);
    assert_eq!(done.spoken, "Sent to build on desktop.");
    assert_eq!(
        calls(&desktop),
        vec![Call::Steer {
            id: "t-build".to_string(),
            message: "run the tests".to_string(),
        }]
    );
    assert_eq!(engine.pending(), None);
}

#[tokio::test]
async fn steering_an_idle_agent_starts_a_turn() {
    let (mut engine, _, laptop) = two_machines();
    engine
        .handle_utterance("ask docs to update the README", false)
        .await;
    let done = engine.confirm().await;
    assert_eq!(
        done.spoken,
        "Docs on laptop was idle, so I started a new turn."
    );
    assert_eq!(
        calls(&laptop),
        vec![Call::Steer {
            id: "t-docs".to_string(),
            message: "update the README".to_string(),
        }]
    );
}

#[tokio::test]
async fn confirm_is_not_taken_while_an_approval_prompt_is_open() {
    let (mut engine, desktop, _) = two_machines();
    engine.handle_utterance("stop build", false).await;
    let outcome = engine
        .handle_utterance("confirm", /*approval_prompt_open*/ true)
        .await;
    assert_eq!(outcome.status, OutcomeStatus::PassThrough);
    assert_eq!(calls(&desktop), Vec::new());
    assert!(engine.pending().is_some());
}

#[tokio::test]
async fn cancel_drops_the_pending_action() {
    let (mut engine, desktop, _) = two_machines();

    engine.handle_utterance("stop build", false).await;
    let cancelled = engine.handle_utterance("never mind", false).await;
    assert_eq!(cancelled.status, OutcomeStatus::Cancelled);
    assert_eq!(cancelled.spoken, "Cancelled.");
    assert_eq!(engine.pending(), None);

    engine.handle_utterance("tell build to deploy", false).await;
    let cancelled = engine.cancel();
    assert_eq!(cancelled.status, OutcomeStatus::Cancelled);
    assert_eq!(
        cancelled.screen,
        "Cancelled: not sent to build on desktop: deploy"
    );

    // Any other utterance cancels too, and still reaches the model.
    engine.handle_utterance("stop build", false).await;
    let other = engine
        .handle_utterance("how does the parser work", false)
        .await;
    assert_eq!(other.status, OutcomeStatus::PassThrough);
    assert_eq!(other.spoken, "Cancelled.");

    assert_eq!(engine.confirm().await.status, OutcomeStatus::Failed);
    assert_eq!(calls(&desktop), Vec::new());
}

#[tokio::test]
async fn pronouns_follow_the_last_agent() {
    let (mut engine, _, laptop) = two_machines();

    // No last agent yet: "it" means nothing.
    let before = engine.handle_utterance("stop it", false).await;
    assert_eq!(before.status, OutcomeStatus::PassThrough);

    let status = engine.handle_utterance("what's tests doing", false).await;
    assert_eq!(
        status.spoken,
        "Tests on laptop: waiting for your approval to run cargo test codex-core."
    );

    let again = engine.handle_utterance("what's it doing", false).await;
    assert_eq!(again.command, status.command);

    let steer = engine
        .handle_utterance("tell it to skip the slow tests", false)
        .await;
    assert_eq!(
        steer.command,
        VoiceCommand::SteerAgent {
            agent: agent_ref("t-tests", "tests", "laptop"),
            message: "skip the slow tests".to_string(),
            needs_confirmation: true,
        }
    );
    engine.cancel();

    // Approval prompt open: "stop it" belongs to the approval grammar.
    let approval = engine.handle_utterance("stop it", true).await;
    assert_eq!(approval.status, OutcomeStatus::PassThrough);
    assert_eq!(engine.pending(), None);

    let stop = engine.handle_utterance("stop that agent", false).await;
    assert_eq!(stop.spoken, "Stop tests on laptop? Say confirm.");
    let done = engine.handle_utterance("confirm", false).await;
    assert_eq!(done.spoken, "Stopped tests on laptop.");
    assert_eq!(
        calls(&laptop),
        vec![Call::Interrupt {
            id: "t-tests".to_string()
        }]
    );

    // A named agent always wins over the pronoun.
    let named = engine.handle_utterance("what's build doing", false).await;
    assert_eq!(
        named.command,
        VoiceCommand::AgentStatus {
            agent: agent_ref("t-build", "build", "desktop"),
        }
    );
    // And "tell me how it works" is still a prompt.
    let prompt = engine.handle_utterance("tell me how it works", false).await;
    assert_eq!(prompt.status, OutcomeStatus::PassThrough);
}

#[tokio::test]
async fn stopping_an_idle_agent_says_so() {
    let (mut engine, _, laptop) = two_machines();

    let known_idle = engine.handle_utterance("stop docs", false).await;
    assert_eq!(known_idle.status, OutcomeStatus::Done);
    assert_eq!(
        known_idle.spoken,
        "Docs on laptop is already idle. Nothing to stop."
    );
    assert_eq!(engine.pending(), None);

    // The snapshot said active, but the turn finished before the confirm.
    engine.handle_utterance("stop tests", false).await;
    laptop.lock().unwrap().agents[0].status = AgentStatus::Idle;
    let finished = engine.confirm().await;
    assert_eq!(
        finished.spoken,
        "Tests on laptop is already idle. Nothing to stop."
    );
}

#[tokio::test]
async fn unreachable_machine_fails_the_command_not_the_engine() {
    let (mut engine, _, _) = two_machines();
    engine.snapshot().await;
    // Status of an agent whose machine has no backend.
    let outcome = engine
        .execute(VoiceCommand::AgentStatus {
            agent: agent_ref("t-x", "x", "server"),
        })
        .await;
    assert_eq!(outcome.status, OutcomeStatus::Failed);
    assert_eq!(outcome.spoken, "I'm not connected to server.");
}

#[tokio::test]
async fn spoken_replies_stay_short() {
    let agents: Vec<AgentInfo> = (0..12)
        .map(|index| {
            running(
                &format!("t-{index}"),
                &format!("refactor-the-entire-authentication-subsystem-{index}"),
                "cargo nextest run --workspace --all-features --no-fail-fast",
            )
        })
        .collect();
    let long_output = "error[E0308]: mismatched types\n".repeat(500);
    let backend = FakeBackend::new("very-long-machine-name-for-testing", agents).with_recent(
        "t-0",
        vec![
            ActivityItem::AgentMessage {
                text: "```diff\n+ added line\n```".to_string(),
            },
            ActivityItem::Command {
                command: "cargo test".to_string(),
                status: ItemStatus::Failed,
                exit_code: Some(101),
                output: Some(long_output),
            },
        ],
    );
    let mut engine = orchestrator(vec![backend]);

    let listed = engine.handle_utterance("list agents", false).await;
    assert_spoken_limit(&listed);
    assert_eq!(listed.screen.lines().count(), 12);

    let status = engine
        .execute(VoiceCommand::AgentStatus {
            agent: agent_ref(
                "t-0",
                "refactor-the-entire-authentication-subsystem-0",
                "very-long-machine-name-for-testing",
            ),
        })
        .await;
    assert_spoken_limit(&status);
    assert!(!status.spoken.contains("E0308"), "{}", status.spoken);
    assert!(!status.spoken.contains("diff"), "{}", status.spoken);
    // The screen shows the tail of the output, never all of it.
    assert_eq!(status.screen.matches("E0308").count(), 5);

    let steer = engine
        .execute(VoiceCommand::SteerAgent {
            agent: agent_ref("t-1", "refactor-the-entire-authentication-subsystem-1", "very-long-machine-name-for-testing"),
            message: "please rewrite every single module in the workspace so that it uses the new error type and then run all of the tests again".to_string(),
            needs_confirmation: true,
        })
        .await;
    assert_spoken_limit(&steer);
}
