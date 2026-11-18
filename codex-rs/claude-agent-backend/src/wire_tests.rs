use super::*;
use pretty_assertions::assert_eq;

fn strings(args: Vec<OsString>) -> Vec<String> {
    args.into_iter()
        .map(|arg| arg.into_string().expect("utf-8"))
        .collect()
}

#[test]
fn new_session_argv_matches_the_sdk() {
    let args = strings(claude_args(
        Some("claude-opus-5-5"),
        Some("high"),
        &Launch::New {
            session_id: "11111111-1111-4111-8111-111111111111".into(),
        },
    ));
    assert_eq!(
        args,
        vec![
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            "--thinking=adaptive",
            "--thinking-display=summarized",
            "--effort=high",
            "--model=claude-opus-5-5",
            "--permission-prompt-tool=stdio",
            "--permission-mode=default",
            "--include-partial-messages",
            "--session-id=11111111-1111-4111-8111-111111111111",
            "--settings",
            r#"{"showThinkingSummaries":true}"#,
        ]
    );
}

#[test]
fn resume_argv_names_the_session_and_omits_a_default_model() {
    let args = strings(claude_args(
        /*model*/ None,
        /*effort*/ None,
        &Launch::Resume {
            session_id: "22222222-2222-4222-8222-222222222222".into(),
        },
    ));
    assert!(args.contains(&"--resume=22222222-2222-4222-8222-222222222222".to_string()));
    assert!(!args.iter().any(|arg| arg.starts_with("--model")));
    assert!(!args.iter().any(|arg| arg.starts_with("--effort")));
    assert!(!args.iter().any(|arg| arg.starts_with("--session-id")));
}

#[test]
fn control_messages_have_the_sdk_shape() {
    assert_eq!(
        control_request("r1", set_model_request(Some("sonnet"))),
        json!({
            "type": "control_request",
            "request_id": "r1",
            "request": { "subtype": "set_model", "model": "sonnet" },
        })
    );
    assert_eq!(
        set_model_request(/*model*/ None),
        json!({ "subtype": "set_model", "model": null })
    );
    assert_eq!(
        set_effort_request(Some("max")),
        json!({ "subtype": "apply_flag_settings", "settings": { "effortLevel": "max" } })
    );
    assert_eq!(
        control_request("r2", interrupt_request()),
        json!({ "type": "control_request", "request_id": "r2", "request": { "subtype": "interrupt" } })
    );
    assert_eq!(
        user_message("hi"),
        json!({
            "type": "user",
            "session_id": "",
            "message": { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
            "parent_tool_use_id": null,
        })
    );
}

#[test]
fn permission_results_have_the_sdk_shape() {
    assert_eq!(
        control_success(
            "c1",
            permission_allow("tu1", json!({"command": "ls"}), None)
        ),
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "c1",
                "response": { "behavior": "allow", "updatedInput": {"command": "ls"}, "toolUseID": "tu1" },
            },
        })
    );
    assert_eq!(
        permission_deny(
            "tu1",
            "User cancelled tool execution.",
            /*interrupt*/ true
        ),
        json!({
            "behavior": "deny",
            "message": "User cancelled tool execution.",
            "toolUseID": "tu1",
            "interrupt": true,
        })
    );
}

#[test]
fn session_rules_reuse_suggestions_or_allow_the_tool() {
    let suggestions = json!([{
        "type": "addRules",
        "rules": [{ "toolName": "Bash", "ruleContent": "cargo test:*" }],
        "behavior": "allow",
        "destination": "localSettings",
    }]);
    assert_eq!(
        session_permission_updates("Bash", Some(&suggestions)),
        vec![json!({
            "type": "addRules",
            "rules": [{ "toolName": "Bash", "ruleContent": "cargo test:*" }],
            "behavior": "allow",
            "destination": "session",
        })]
    );
    assert_eq!(
        session_permission_updates("Edit", /*suggestions*/ None),
        vec![json!({
            "type": "addRules",
            "rules": [{ "toolName": "Edit" }],
            "behavior": "allow",
            "destination": "session",
        })]
    );
}
