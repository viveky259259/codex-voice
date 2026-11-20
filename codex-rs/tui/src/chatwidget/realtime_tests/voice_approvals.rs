//! Spoken approval answers, their confirmation step, and spoken status prompts.

use super::*;
use crate::bottom_pane::ApplyPatchApprovalRequest;
use crate::bottom_pane::ApprovalRequest;
use crate::bottom_pane::ExecApprovalRequest;
use crate::diff_model::FileChange;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_utils_absolute_path::AbsolutePathBuf;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedReceiver;

pub(super) const ALL_DECISIONS: [CommandExecutionApprovalDecision; 4] = [
    CommandExecutionApprovalDecision::Accept,
    CommandExecutionApprovalDecision::AcceptForSession,
    CommandExecutionApprovalDecision::Decline,
    CommandExecutionApprovalDecision::Cancel,
];

pub(super) fn exec_approval(
    thread_id: ThreadId,
    id: &str,
    command: &str,
    decisions: &[CommandExecutionApprovalDecision],
) -> ApprovalRequest {
    ApprovalRequest::Exec(ExecApprovalRequest {
        kind: Default::default(),
        thread_id,
        thread_label: None,
        id: id.to_string(),
        environment_id: None,
        command: vec!["bash".to_string(), "-lc".to_string(), command.to_string()],
        reason: None,
        available_decisions: decisions.to_vec(),
        network_approval_context: None,
        additional_permissions: None,
    })
}

pub(super) fn spoken(ops: &mut UnboundedReceiver<AppCommand>) -> Vec<String> {
    std::iter::from_fn(|| ops.try_recv().ok())
        .filter_map(|op| match op {
            AppCommand::RealtimeConversationSpeech { text, .. } => Some(text.as_str().to_string()),
            _ => None,
        })
        .collect()
}

/// Drain app events, returning approval submissions and rendered history text.
pub(super) fn drain_events(
    chat: &mut ChatWidget,
    events: &mut UnboundedReceiver<AppEvent>,
) -> (Vec<AppCommand>, String) {
    commit_realtime_history_events(chat, events);
    let mut submitted = Vec::new();
    let mut history = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            AppEvent::SubmitThreadOp { op, .. } => submitted.push(op),
            AppEvent::InsertHistoryCell(cell) => history.extend(
                cell.display_lines(/*width*/ 100)
                    .into_iter()
                    .map(|line| line.to_string()),
            ),
            _ => {}
        }
    }
    (submitted, history.join("\n"))
}

pub(super) fn say(chat: &mut ChatWidget, text: &str) {
    chat.on_realtime_transcript_delta("user".to_string(), text.to_string());
    chat.on_realtime_transcript_done("user".to_string(), text.to_string());
}

pub(super) fn exec_decision(op: &AppCommand) -> Option<(&str, &CommandExecutionApprovalDecision)> {
    match op {
        AppCommand::ExecApproval { id, decision, .. } => Some((id.as_str(), decision)),
        _ => None,
    }
}

#[tokio::test]
async fn approval_request_is_spoken_briefly_during_voice() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);

    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-1",
        "cargo test -p codex-tui",
        &ALL_DECISIONS,
    ));

    assert_eq!(
        spoken(&mut ops),
        vec!["Codex wants to run cargo test -p codex-tui. Say approve or deny.".to_string()]
    );
    // A queued prompt is announced only when it becomes visible.
    chat.push_approval_request(exec_approval(thread_id, "exec-2", "ls", &ALL_DECISIONS));
    assert_eq!(spoken(&mut ops), Vec::<String>::new());
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty());
}

#[tokio::test]
async fn patch_approval_speech_counts_files_without_reading_the_diff() {
    let (mut chat, _sender, _events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    let cwd = AbsolutePathBuf::from_absolute_path(std::env::temp_dir()).expect("absolute");
    let changes: HashMap<PathBuf, FileChange> = ["a.rs", "b.rs", "c.rs"]
        .into_iter()
        .map(|name| {
            (
                PathBuf::from(name),
                FileChange::Add {
                    content: "PRIVATE DIFF LINE\n".repeat(100),
                },
            )
        })
        .collect();

    chat.push_approval_request(ApprovalRequest::ApplyPatch(ApplyPatchApprovalRequest {
        thread_id,
        thread_label: None,
        id: "patch-1".to_string(),
        reason: None,
        cwd,
        changes,
    }));

    assert_eq!(
        spoken(&mut ops),
        vec!["Codex wants to edit 3 files. Say approve or deny.".to_string()]
    );
}

#[tokio::test]
async fn approval_speech_requires_an_active_voice_conversation() {
    let (mut chat, _sender, _events, mut ops) = make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(ThreadId::new());

    chat.push_approval_request(exec_approval(
        chat.thread_id.unwrap_or_default(),
        "exec-1",
        "ls",
        &ALL_DECISIONS,
    ));

    assert!(ops.try_recv().is_err());
}

#[tokio::test]
async fn spoken_approve_waits_for_confirm_then_applies_once() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-1",
        "cargo test",
        &ALL_DECISIONS,
    ));
    spoken(&mut ops);

    say(&mut chat, "Approve.");
    assert_eq!(
        spoken(&mut ops),
        vec!["Approve cargo test? Say confirm.".to_string()]
    );
    let (submitted, history) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "approve alone must not apply");
    assert!(
        history.contains("Approve cargo test? Say confirm."),
        "{history}"
    );
    assert!(chat.bottom_pane.current_approval_request().is_some());

    say(&mut chat, "Yes, confirm.");
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert_eq!(
        submitted
            .iter()
            .filter_map(exec_decision)
            .collect::<Vec<_>>(),
        vec![("exec-1", &CommandExecutionApprovalDecision::Accept)]
    );
    assert!(chat.bottom_pane.current_approval_request().is_none());
    assert!(
        chat.realtime_conversation
            .voice_controls
            .voice_approval
            .is_none()
    );
    assert_eq!(spoken(&mut ops), Vec::<String>::new());
}

#[tokio::test]
async fn spoken_session_approval_is_confirmed_and_only_when_offered() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-1",
        "make lint",
        &ALL_DECISIONS,
    ));
    spoken(&mut ops);

    say(&mut chat, "always allow");
    assert_eq!(
        spoken(&mut ops),
        vec!["Approve make lint for this session? Say confirm.".to_string()]
    );
    say(&mut chat, "confirm");
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert_eq!(
        submitted
            .iter()
            .filter_map(exec_decision)
            .collect::<Vec<_>>(),
        vec![(
            "exec-1",
            &CommandExecutionApprovalDecision::AcceptForSession
        )]
    );

    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-2",
        "make lint",
        &[
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::Cancel,
        ],
    ));
    spoken(&mut ops);
    say(&mut chat, "approve for this session");
    assert_eq!(
        spoken(&mut ops),
        vec!["That option is not offered. Say approve or deny.".to_string()]
    );
    say(&mut chat, "confirm");
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty());
}

#[tokio::test]
async fn any_other_utterance_cancels_a_pending_voice_approval() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-1",
        "rm -rf build",
        &ALL_DECISIONS,
    ));
    spoken(&mut ops);

    say(&mut chat, "approve");
    spoken(&mut ops);
    say(&mut chat, "what does that command delete exactly");
    assert_eq!(spoken(&mut ops), vec!["Approval cancelled.".to_string()]);
    assert!(
        chat.realtime_conversation
            .voice_controls
            .voice_approval
            .is_none()
    );

    // A late "confirm" cannot resurrect the cancelled approval.
    say(&mut chat, "confirm");
    assert_eq!(
        spoken(&mut ops),
        vec!["Say approve first, then confirm.".to_string()]
    );
    let (submitted, history) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "{submitted:?}");
    assert!(history.contains("Voice approval cancelled"), "{history}");
    assert!(chat.bottom_pane.current_approval_request().is_some());

    // The keyboard still answers the prompt.
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert_eq!(
        submitted
            .iter()
            .filter_map(exec_decision)
            .collect::<Vec<_>>(),
        vec![("exec-1", &CommandExecutionApprovalDecision::Accept)]
    );
}

#[tokio::test]
async fn a_confirmation_only_applies_to_the_prompt_it_was_armed_for() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(thread_id, "exec-1", "ls", &ALL_DECISIONS));
    say(&mut chat, "approve");
    // The prompt is answered elsewhere and a different one replaces it.
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-2",
        "git push --force",
        &ALL_DECISIONS,
    ));
    drain_events(&mut chat, &mut events);
    spoken(&mut ops);

    say(&mut chat, "confirm");
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "{submitted:?}");
    assert_eq!(
        spoken(&mut ops),
        vec!["Say approve first, then confirm.".to_string()]
    );
}

#[tokio::test]
async fn spoken_refusals_apply_immediately_without_confirmation() {
    for (utterance, expected, reply) in [
        ("No.", CommandExecutionApprovalDecision::Decline, "Denied."),
        ("deny", CommandExecutionApprovalDecision::Decline, "Denied."),
        (
            "reject",
            CommandExecutionApprovalDecision::Decline,
            "Denied.",
        ),
        (
            "Stop!",
            CommandExecutionApprovalDecision::Cancel,
            "Stopped.",
        ),
    ] {
        let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
        let thread_id = activate_voice(&mut chat);
        chat.push_approval_request(exec_approval(thread_id, "exec-1", "ls", &ALL_DECISIONS));
        spoken(&mut ops);

        say(&mut chat, utterance);

        let (submitted, _) = drain_events(&mut chat, &mut events);
        assert_eq!(
            submitted
                .iter()
                .filter_map(exec_decision)
                .collect::<Vec<_>>(),
            vec![("exec-1", &expected)],
            "{utterance}"
        );
        assert_eq!(spoken(&mut ops), vec![reply.to_string()], "{utterance}");
        assert!(chat.bottom_pane.current_approval_request().is_none());
    }
}

#[tokio::test]
async fn a_refusing_option_named_by_number_letter_or_label_applies_at_once() {
    for utterance in [
        "Option 3.",
        "option d",
        "D.",
        "the third one",
        "No, continue without running it.",
    ] {
        let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
        let thread_id = activate_voice(&mut chat);
        chat.push_approval_request(exec_approval(thread_id, "exec-1", "ls", &ALL_DECISIONS));
        spoken(&mut ops);

        say(&mut chat, utterance);

        let (submitted, _) = drain_events(&mut chat, &mut events);
        assert_eq!(
            submitted
                .iter()
                .filter_map(exec_decision)
                .collect::<Vec<_>>(),
            vec![("exec-1", &CommandExecutionApprovalDecision::Decline)],
            "{utterance}"
        );
        assert_eq!(
            spoken(&mut ops),
            vec!["Option 3, No, continue without running it.".to_string()],
            "{utterance}"
        );
        assert!(chat.bottom_pane.current_approval_request().is_none());
    }
}

#[tokio::test]
async fn a_granting_option_waits_for_confirm() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(thread_id, "exec-1", "ls", &ALL_DECISIONS));
    spoken(&mut ops);

    say(&mut chat, "option a");
    let prompt =
        "Option 2, Yes, and don't ask again for this command in this session? Say confirm.";
    assert_eq!(spoken(&mut ops), vec![prompt.to_string()]);
    let (submitted, history) = drain_events(&mut chat, &mut events);
    assert!(
        submitted.is_empty(),
        "an option that grants must not apply alone"
    );
    assert!(history.contains(prompt), "{history}");

    say(&mut chat, "confirm");
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert_eq!(
        submitted
            .iter()
            .filter_map(exec_decision)
            .collect::<Vec<_>>(),
        vec![(
            "exec-1",
            &CommandExecutionApprovalDecision::AcceptForSession
        )]
    );
    assert!(chat.bottom_pane.current_approval_request().is_none());
}

#[tokio::test]
async fn deny_falls_back_to_abort_when_decline_is_not_offered() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-1",
        "ls",
        &[
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::Cancel,
        ],
    ));

    say(&mut chat, "deny");

    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert_eq!(
        submitted
            .iter()
            .filter_map(exec_decision)
            .collect::<Vec<_>>(),
        vec![("exec-1", &CommandExecutionApprovalDecision::Cancel)]
    );
}

#[tokio::test]
async fn negated_and_long_phrases_are_not_approval_answers() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(thread_id, "exec-1", "ls", &ALL_DECISIONS));
    spoken(&mut ops);

    for utterance in [
        "Don't approve.",
        "do not approve that",
        "never allow it",
        "approve it after you explain what it does",
    ] {
        say(&mut chat, utterance);
        assert_eq!(spoken(&mut ops), Vec::<String>::new(), "{utterance}");
        assert!(
            chat.realtime_conversation
                .voice_controls
                .voice_approval
                .is_none()
        );
        assert!(
            chat.realtime_conversation
                .voice_controls
                .consumed_voice_controls
                .is_empty()
        );
    }
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "{submitted:?}");
    assert!(chat.bottom_pane.current_approval_request().is_some());
}

#[tokio::test]
async fn approval_utterances_are_not_treated_as_new_voice_requests() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    // A spoken request is running and waits on a command approval.
    let turn_id = "voice-turn";
    chat.turn_lifecycle.last_turn_id = Some(turn_id.to_string());
    let delegation =
        user_item("<realtime_delegation><input>run the tests</input></realtime_delegation>");
    start_item(&mut chat, thread_id, turn_id, delegation.clone());
    complete_item(&mut chat, thread_id, turn_id, delegation);
    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-1",
        "cargo test",
        &ALL_DECISIONS,
    ));
    spoken(&mut ops);

    say(&mut chat, "Approve.");
    say(&mut chat, "Confirm.");
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert_eq!(submitted.iter().filter_map(exec_decision).count(), 1);
    spoken(&mut ops);
    let generation = chat.realtime_conversation.input_generation;

    // If the voice model hands the answer off anyway, it is not a new request:
    // it never takes over speech or bumps the input generation, and stays hidden.
    for (handoff_turn, text) in [
        ("handoff-approve", "Approve."),
        ("handoff-confirm", "Confirm."),
    ] {
        start_item(
            &mut chat,
            thread_id,
            handoff_turn,
            user_item(&format!(
                "<realtime_delegation><input>{text}</input></realtime_delegation>"
            )),
        );
        assert_eq!(chat.realtime_conversation.input_generation, generation);
        assert!(matches!(
            chat.realtime_conversation.turn_origins.get(handoff_turn),
            Some(super::super::RealtimeTurnOrigin::Delegated {
                may_speak: false,
                ..
            })
        ));
        assert!(chat.should_hide_realtime_delegation(handoff_turn));
    }
    assert!(
        chat.realtime_conversation
            .voice_controls
            .consumed_voice_controls
            .is_empty()
    );

    // The original voice request still speaks its answer after the approval.
    let answer = agent_item(
        "final-answer",
        "All tests passed.",
        Some(MessagePhase::FinalAnswer),
    );
    start_item(&mut chat, thread_id, turn_id, answer.clone());
    complete_item(&mut chat, thread_id, turn_id, answer.clone());
    finish_turn(
        &mut chat,
        thread_id,
        turn_id,
        vec![answer],
        TurnStatus::Completed,
    );
    assert_eq!(spoken(&mut ops), vec!["All tests passed.".to_string()]);
}

#[tokio::test]
async fn ordinary_speech_after_an_answer_is_still_handed_off_normally() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(thread_id, "exec-1", "ls", &ALL_DECISIONS));
    say(&mut chat, "no");
    drain_events(&mut chat, &mut events);
    spoken(&mut ops);
    // The user moves on; their next words are a real request.
    say(&mut chat, "never mind, what time is it");
    assert!(
        chat.realtime_conversation
            .voice_controls
            .consumed_voice_controls
            .is_empty()
    );

    // Without a visible prompt, a short "no" handoff is ordinary voice input.
    start_item(
        &mut chat,
        thread_id,
        "later-turn",
        user_item("<realtime_delegation><input>no</input></realtime_delegation>"),
    );
    assert!(matches!(
        chat.realtime_conversation.turn_origins.get("later-turn"),
        Some(super::super::RealtimeTurnOrigin::Delegated {
            may_speak: true,
            ..
        })
    ));
}

#[tokio::test]
async fn a_failed_turn_is_summarized_aloud() {
    let (mut chat, _sender, _events, mut ops) = make_chatwidget_manual_with_sender().await;
    activate_voice(&mut chat);

    chat.handle_non_retry_error(
        "stream disconnected before completion. Please retry.\n{\"raw\": \"payload\"}".to_string(),
        /*codex_error_info*/ None,
    );

    assert_eq!(
        spoken(&mut ops),
        vec!["Codex hit an error: stream disconnected before completion.".to_string()]
    );
}

/// The voice model handing the user's words off as a delegated turn.
pub(super) fn hand_off(chat: &mut ChatWidget, thread_id: ThreadId, turn_id: &str, text: &str) {
    start_item(
        chat,
        thread_id,
        turn_id,
        user_item(&format!(
            "<realtime_delegation><input>{text}</input></realtime_delegation>"
        )),
    );
}

#[tokio::test]
async fn one_spoken_refusal_answers_one_prompt_whichever_copy_arrives_first() {
    for handoff_first in [true, false] {
        let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
        let thread_id = activate_voice(&mut chat);
        chat.push_approval_request(exec_approval(thread_id, "exec-1", "ls", &ALL_DECISIONS));
        chat.push_approval_request(exec_approval(thread_id, "exec-2", "pwd", &ALL_DECISIONS));
        spoken(&mut ops);

        if handoff_first {
            hand_off(&mut chat, thread_id, "handoff-no", "no");
            say(&mut chat, "No.");
        } else {
            say(&mut chat, "No.");
            hand_off(&mut chat, thread_id, "handoff-no", "no");
        }

        let (submitted, _) = drain_events(&mut chat, &mut events);
        assert_eq!(
            submitted
                .iter()
                .filter_map(exec_decision)
                .collect::<Vec<_>>(),
            vec![("exec-1", &CommandExecutionApprovalDecision::Decline)],
            "handoff_first={handoff_first}"
        );
        assert!(
            chat.bottom_pane.current_approval_request().is_some(),
            "exec-2 must still be waiting, handoff_first={handoff_first}"
        );
        assert!(chat.should_hide_realtime_delegation("handoff-no"));
    }
}

#[tokio::test]
async fn two_spoken_refusals_answer_two_prompts() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(thread_id, "exec-1", "ls", &ALL_DECISIONS));
    chat.push_approval_request(exec_approval(thread_id, "exec-2", "pwd", &ALL_DECISIONS));
    spoken(&mut ops);

    // Both transcripts arrive before either handoff.
    say(&mut chat, "no");
    say(&mut chat, "no");
    hand_off(&mut chat, thread_id, "handoff-1", "no");
    hand_off(&mut chat, thread_id, "handoff-2", "no");

    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert_eq!(
        submitted
            .iter()
            .filter_map(exec_decision)
            .collect::<Vec<_>>(),
        vec![
            ("exec-1", &CommandExecutionApprovalDecision::Decline),
            ("exec-2", &CommandExecutionApprovalDecision::Decline),
        ]
    );
    assert!(chat.should_hide_realtime_delegation("handoff-1"));
    assert!(chat.should_hide_realtime_delegation("handoff-2"));
}

#[tokio::test]
async fn handoff_first_approval_is_confirmed_once_by_the_transcript() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-1",
        "cargo test",
        &ALL_DECISIONS,
    ));
    chat.push_approval_request(exec_approval(thread_id, "exec-2", "pwd", &ALL_DECISIONS));
    spoken(&mut ops);

    hand_off(&mut chat, thread_id, "handoff-approve", "Approve.");
    assert_eq!(spoken(&mut ops), Vec::<String>::new());
    say(&mut chat, "Approve.");
    assert_eq!(
        spoken(&mut ops),
        vec!["Approve cargo test? Say confirm.".to_string()]
    );

    hand_off(&mut chat, thread_id, "handoff-confirm", "Confirm.");
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "{submitted:?}");
    say(&mut chat, "Confirm.");

    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert_eq!(
        submitted
            .iter()
            .filter_map(exec_decision)
            .collect::<Vec<_>>(),
        vec![("exec-1", &CommandExecutionApprovalDecision::Accept)]
    );
    let speech = spoken(&mut ops);
    assert!(
        !speech.iter().any(|line| line.contains("Say approve first")),
        "{speech:?}"
    );
    for turn in ["handoff-approve", "handoff-confirm"] {
        assert!(chat.should_hide_realtime_delegation(turn), "{turn}");
    }
}

#[tokio::test]
async fn a_model_authored_confirm_never_approves() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-1",
        "git push --force",
        &ALL_DECISIONS,
    ));
    say(&mut chat, "approve");
    drain_events(&mut chat, &mut events);
    spoken(&mut ops);

    // The model says "confirm" on the user's behalf; the user never did.
    hand_off(&mut chat, thread_id, "handoff-confirm", "confirm");

    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "{submitted:?}");
    assert!(chat.bottom_pane.current_approval_request().is_some());
}

#[tokio::test]
async fn a_refusal_that_cannot_apply_says_so() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = activate_voice(&mut chat);
    chat.push_approval_request(exec_approval(
        thread_id,
        "exec-1",
        "ls",
        &[CommandExecutionApprovalDecision::Accept],
    ));
    spoken(&mut ops);

    say(&mut chat, "no");

    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "{submitted:?}");
    assert_eq!(
        spoken(&mut ops),
        vec!["That option is no longer available.".to_string()]
    );
}

/// A command approval from `build` on `devbox`, shown under a local alias.
fn remote_exec_approval(alias: ThreadId, id: &str, command: &str) -> ApprovalRequest {
    let ApprovalRequest::Exec(mut request) = exec_approval(alias, id, command, &ALL_DECISIONS)
    else {
        unreachable!("exec_approval builds an exec request");
    };
    request.thread_label = Some(crate::bottom_pane::remote_thread_label("devbox", "build"));
    ApprovalRequest::Exec(request)
}

#[tokio::test]
async fn a_remote_approval_names_its_machine_and_needs_confirm() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    activate_voice(&mut chat);
    let alias = ThreadId::new();

    chat.push_approval_request(remote_exec_approval(alias, "exec-1", "cargo test"));
    assert_eq!(
        spoken(&mut ops),
        vec!["Build on devbox wants to run cargo test. Say approve or deny.".to_string()]
    );

    say(&mut chat, "approve");
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "{submitted:?}");
    assert_eq!(
        spoken(&mut ops),
        vec!["Approve cargo test for build on devbox? Say confirm.".to_string()]
    );

    say(&mut chat, "confirm");
    let mut submitted = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let AppEvent::SubmitThreadOp { thread_id, op } = event {
            submitted.push((thread_id, op));
        }
    }
    assert_eq!(
        submitted
            .iter()
            .map(|(thread_id, op)| (*thread_id, exec_decision(op)))
            .collect::<Vec<_>>(),
        vec![(
            alias,
            Some(("exec-1", &CommandExecutionApprovalDecision::Accept))
        )]
    );
}

#[tokio::test]
async fn a_remote_denial_applies_at_once() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    activate_voice(&mut chat);
    chat.push_approval_request(remote_exec_approval(
        ThreadId::new(),
        "exec-1",
        "rm -rf target",
    ));
    spoken(&mut ops);

    say(&mut chat, "deny");

    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert_eq!(
        submitted
            .iter()
            .filter_map(exec_decision)
            .collect::<Vec<_>>(),
        vec![("exec-1", &CommandExecutionApprovalDecision::Decline)]
    );
    assert_eq!(spoken(&mut ops), vec!["Denied.".to_string()]);
}

#[tokio::test]
async fn an_approval_answered_elsewhere_cancels_its_pending_confirm() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    activate_voice(&mut chat);
    let alias = ThreadId::new();
    chat.push_approval_request(remote_exec_approval(alias, "exec-1", "cargo test"));
    chat.push_approval_request(remote_exec_approval(alias, "exec-2", "cargo build"));
    say(&mut chat, "approve");
    drain_events(&mut chat, &mut events);
    spoken(&mut ops);

    // Someone at devbox answered the first prompt; the second shows and is said.
    chat.dismiss_app_server_request(
        &crate::app::app_server_requests::ResolvedAppServerRequest::ExecApproval {
            thread_id: alias.to_string(),
            id: "exec-1".to_string(),
        },
    );
    assert_eq!(
        spoken(&mut ops),
        vec!["Build on devbox wants to run cargo build. Say approve or deny.".to_string()]
    );

    // The "approve" said for the first prompt cannot be confirmed onto the second.
    say(&mut chat, "confirm");
    let (submitted, _) = drain_events(&mut chat, &mut events);
    assert!(submitted.is_empty(), "{submitted:?}");
    assert_eq!(
        spoken(&mut ops),
        vec!["Say approve first, then confirm.".to_string()]
    );
}
