use super::*;
use crate::bottom_pane::ApplyPatchApprovalRequest;
use crate::bottom_pane::ExecApprovalRequest;
use crate::diff_model::FileChange;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::PathBuf;

fn exec_request(command: &[&str]) -> ApprovalRequest {
    ApprovalRequest::Exec(ExecApprovalRequest {
        kind: Default::default(),
        thread_id: ThreadId::new(),
        thread_label: None,
        id: "exec-1".to_string(),
        environment_id: None,
        command: command.iter().map(ToString::to_string).collect(),
        reason: None,
        available_decisions: vec![
            CommandExecutionApprovalDecision::Accept,
            CommandExecutionApprovalDecision::Cancel,
        ],
        network_approval_context: None,
        additional_permissions: None,
    })
}

fn patch_request(paths: &[&str]) -> ApprovalRequest {
    let cwd = AbsolutePathBuf::from_absolute_path(std::env::temp_dir()).expect("absolute temp dir");
    ApprovalRequest::ApplyPatch(ApplyPatchApprovalRequest {
        thread_id: ThreadId::new(),
        thread_label: None,
        id: "patch-1".to_string(),
        reason: None,
        cwd,
        changes: paths
            .iter()
            .map(|path| {
                (
                    PathBuf::from(path),
                    FileChange::Add {
                        content: "secret diff body\n".repeat(50),
                    },
                )
            })
            .collect::<HashMap<_, _>>(),
    })
}

#[test]
fn approve_phrases_map_to_approve_once() {
    for utterance in [
        "approve",
        "Approve.",
        "yes",
        "Yes!",
        "allow",
        "go ahead",
        "Yes, go ahead.",
        "okay, approve it please",
        "Approve’s",
    ] {
        let expected = (utterance != "Approve’s").then_some(VoiceApprovalIntent::Approve);
        assert_eq!(
            parse_voice_approval_intent(utterance),
            expected,
            "{utterance}"
        );
    }
}

#[test]
fn session_phrases_map_to_session_approval() {
    for utterance in [
        "approve for session",
        "Approve for this session.",
        "always allow",
        "Always allow it, please.",
    ] {
        assert_eq!(
            parse_voice_approval_intent(utterance),
            Some(VoiceApprovalIntent::ApproveForSession),
            "{utterance}"
        );
    }
}

#[test]
fn refusal_phrases_map_to_deny_or_abort() {
    for utterance in [
        "deny",
        "No.",
        "nope",
        "reject",
        "Decline it",
        "no, thank you",
    ] {
        assert_eq!(
            parse_voice_approval_intent(utterance),
            Some(VoiceApprovalIntent::Deny),
            "{utterance}"
        );
    }
    for utterance in ["stop", "Cancel.", "abort", "No, stop!"] {
        assert_eq!(
            parse_voice_approval_intent(utterance),
            Some(VoiceApprovalIntent::Abort),
            "{utterance}"
        );
    }
}

#[test]
fn confirm_phrases_are_only_explicit_confirmations() {
    for utterance in ["confirm", "Yes, confirm.", "Confirmed", "I confirm"] {
        assert_eq!(
            parse_voice_approval_intent(utterance),
            Some(VoiceApprovalIntent::Confirm),
            "{utterance}"
        );
    }
    // A bare "yes" is an approve intent, not a confirmation.
    assert_eq!(
        parse_voice_approval_intent("yes"),
        Some(VoiceApprovalIntent::Approve)
    );
}

#[test]
fn negated_or_hedged_phrases_are_ignored() {
    for utterance in [
        "don't approve",
        "Don’t approve it",
        "do not approve",
        "not yet",
        "never allow",
        "I can't confirm",
        "don't confirm",
        "wait, approve",
        "maybe approve",
        "approve if it is safe",
        "yes but not now",
    ] {
        assert_eq!(parse_voice_approval_intent(utterance), None, "{utterance}");
    }
}

#[test]
fn approval_words_inside_longer_requests_are_ignored() {
    for utterance in [
        "",
        "   ",
        "approve the migration and then run the full test suite",
        "can you explain what this command does",
        "yes and also update the readme",
        "approved changes look good to me overall",
        "go ahead and delete everything",
        "approval",
        "yesterday",
        "denying",
        "stopwatch",
    ] {
        assert_eq!(parse_voice_approval_intent(utterance), None, "{utterance}");
    }
}

#[test]
fn prompts_describe_commands_briefly() {
    let request = exec_request(&["bash", "-lc", "cargo test -p codex-tui"]);
    assert_eq!(
        approval_request_prompt(&request),
        "Codex wants to run cargo test -p codex-tui. Say approve or deny."
    );
    assert_eq!(
        approval_confirmation_prompt(&request, VoiceApprovalIntent::Approve),
        "Approve cargo test -p codex-tui? Say confirm."
    );
    assert_eq!(
        approval_confirmation_prompt(&request, VoiceApprovalIntent::ApproveForSession),
        "Approve cargo test -p codex-tui for this session? Say confirm."
    );
}

#[test]
fn long_commands_are_truncated_before_speaking() {
    let long = "python3 -c 'import os; [os.remove(p) for p in os.listdir(\".\")]' && echo done && echo again";
    let request = exec_request(&["bash", "-lc", long]);
    let prompt = approval_request_prompt(&request);
    assert!(
        prompt.split_whitespace().count() <= MAX_SPOKEN_WORDS,
        "{prompt}"
    );
    assert!(prompt.contains('…'), "{prompt}");
    assert!(!prompt.contains("echo again"), "{prompt}");

    let multiline = exec_request(&["bash", "-lc", "echo one\necho two\necho three"]);
    assert_eq!(
        approval_request_prompt(&multiline),
        "Codex wants to run echo one… Say approve or deny."
    );
}

#[test]
fn patch_prompts_count_files_and_never_read_diffs() {
    let many = patch_request(&["/tmp/a.rs", "/tmp/b.rs", "/tmp/c.rs"]);
    assert_eq!(
        approval_request_prompt(&many),
        "Codex wants to edit 3 files. Say approve or deny."
    );
    let one = patch_request(&["/tmp/src/main.rs"]);
    let prompt = approval_request_prompt(&one);
    assert_eq!(prompt, "Codex wants to edit main.rs. Say approve or deny.");
    assert!(!prompt.contains("secret diff body"));
    assert_eq!(
        approval_confirmation_prompt(&many, VoiceApprovalIntent::Approve),
        "Approve edits to 3 files? Say confirm."
    );
}

#[test]
fn turn_errors_are_summarized_in_one_short_sentence() {
    assert_eq!(
        turn_error_speech("stream disconnected before completion. Retry later.\nraw details"),
        "Codex hit an error: stream disconnected before completion."
    );
    assert_eq!(turn_error_speech("  \n"), "Codex hit an error.");
    let long = "word ".repeat(80);
    let spoken = turn_error_speech(&long);
    assert!(
        spoken.split_whitespace().count() <= MAX_SPOKEN_WORDS,
        "{spoken}"
    );
}

#[test]
fn limit_words_marks_truncation() {
    assert_eq!(limit_words("a b c", 3), "a b c");
    assert_eq!(limit_words("a  b c d", 2), "a b…");
}
