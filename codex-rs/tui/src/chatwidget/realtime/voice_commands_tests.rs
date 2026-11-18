//! Pure voice-command routing: which transcripts are kept from the model, and
//! the pending state predicted for the orchestrator.

use super::*;
use codex_orchestrator::Outcome;
use pretty_assertions::assert_eq;

fn agent(id: &str, name: &str, machine: &str) -> AgentRef {
    AgentRef {
        id: id.to_string(),
        name: name.to_string(),
        machine: Some(machine.to_string()),
    }
}

fn state() -> VoiceCommandState {
    VoiceCommandState {
        context: VoiceContext {
            known_agents: vec![
                agent("t-build-d", "build", "desktop"),
                agent("t-build-l", "build", "laptop"),
                agent("t-lint", "lint", "desktop"),
            ],
            known_machines: vec!["desktop".to_string(), "laptop".to_string()],
            known_models: Vec::new(),
        },
        ..VoiceCommandState::default()
    }
}

fn steer_lint() -> VoiceCommand {
    VoiceCommand::SteerAgent {
        agent: agent("t-lint", "lint", "desktop"),
        message: "run the tests".to_string(),
        needs_confirmation: true,
    }
}

#[test]
fn only_commands_are_run() {
    let mut commands = state();
    for (utterance, route) in [
        ("what's running", VoiceCommandRoute::Run),
        ("what's lint doing", VoiceCommandRoute::Run),
        (
            "tell me how this function works",
            VoiceCommandRoute::PassThrough,
        ),
        ("stop", VoiceCommandRoute::PassThrough),
        ("confirm", VoiceCommandRoute::PassThrough),
    ] {
        assert_eq!(commands.route(utterance, false), route, "{utterance}");
        assert_eq!(commands.pending, None, "{utterance}");
    }
}

#[test]
fn a_steer_waits_for_confirm_and_anything_else_cancels_it() {
    let mut commands = state();
    assert_eq!(
        commands.route("tell lint to run the tests", false),
        VoiceCommandRoute::Run
    );
    assert_eq!(
        commands.pending,
        Some(PendingVoiceCommand::Confirmation(steer_lint()))
    );
    assert_eq!(commands.route("yes confirm", false), VoiceCommandRoute::Run);
    assert_eq!(commands.pending, None);

    commands.route("tell lint to run the tests", false);
    assert_eq!(
        commands.route("what time is it", false),
        VoiceCommandRoute::CancelThenPassThrough {
            confirmation_cancelled: true
        }
    );
    assert_eq!(commands.pending, None);

    commands.route("tell lint to run the tests", false);
    assert_eq!(commands.route("never mind", false), VoiceCommandRoute::Run);
    assert_eq!(commands.pending, None);
}

#[test]
fn confirm_belongs_to_an_open_approval_prompt() {
    let mut commands = state();
    commands.route("tell lint to run the tests", false);
    assert_eq!(
        commands.route("confirm", true),
        VoiceCommandRoute::PassThrough
    );
    assert_eq!(
        commands.pending,
        Some(PendingVoiceCommand::Confirmation(steer_lint()))
    );
}

#[test]
fn an_ambiguous_target_takes_the_next_answer() {
    let mut commands = state();
    assert_eq!(
        commands.route("what's build doing", false),
        VoiceCommandRoute::Run
    );
    assert!(matches!(
        commands.pending,
        Some(PendingVoiceCommand::Choice { .. })
    ));
    assert_eq!(
        commands.route("the second one", false),
        VoiceCommandRoute::Run
    );
    assert_eq!(commands.pending, None);
    assert_eq!(
        commands.last_agent,
        Some(agent("t-build-l", "build", "laptop"))
    );
    // "it" now resolves locally.
    assert_eq!(
        commands.parse("what's it doing", false),
        VoiceCommand::AgentStatus {
            agent: agent("t-build-l", "build", "laptop")
        }
    );
}

#[test]
fn an_unanswered_question_is_dropped_silently() {
    let mut commands = state();
    commands.route("what's build doing", false);
    assert_eq!(
        commands.route("what time is it", false),
        VoiceCommandRoute::CancelThenPassThrough {
            confirmation_cancelled: false
        }
    );
    assert_eq!(commands.pending, None);
}

#[test]
fn only_the_latest_answer_corrects_the_pending_state() {
    let mut commands = state();
    commands.route("stop lint", false);
    let stop_seq = commands.next_request();
    let newer = commands.next_request();
    commands.pending = None;
    let stale = VoiceCommandResult {
        seq: stop_seq,
        outcome: Some(Outcome {
            status: OutcomeStatus::AwaitingConfirmation,
            spoken: String::new(),
            screen: String::new(),
            command: steer_lint(),
        }),
        spoken_override: None,
        context: VoiceContext::default(),
        last_agent: None,
    };
    commands.apply_result(&stale);
    assert_eq!(commands.pending, None);
    // The context always follows the engine.
    assert_eq!(commands.context, VoiceContext::default());

    let current = VoiceCommandResult {
        seq: newer,
        ..stale
    };
    commands.apply_result(&current);
    assert_eq!(
        commands.pending,
        Some(PendingVoiceCommand::Confirmation(steer_lint()))
    );
}

#[test]
fn handoffs_are_claimed_without_changing_anything() {
    let mut commands = state();
    assert!(commands.claims_handoff("what's lint doing", false));
    assert!(!commands.claims_handoff("confirm", false));
    assert!(!commands.claims_handoff("tell me a joke", false));

    commands.route("tell lint to run the tests", false);
    let before = commands.pending.clone();
    for answer in ["confirm", "yes", "no", "cancel"] {
        assert!(commands.claims_handoff(answer, false), "{answer}");
    }
    assert_eq!(commands.pending, before);
}

#[test]
fn a_run_transcript_claims_its_echo_once() {
    let mut commands = state();
    commands.route("tell lint to run the tests", false);
    commands.remember_consumed("Confirm.");
    commands.pending = None;
    assert!(commands.claims_handoff("confirm", false));
    assert!(!commands.claims_handoff("confirm", false));
}
