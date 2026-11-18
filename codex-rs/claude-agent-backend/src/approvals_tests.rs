use super::*;
use codex_app_server_protocol::GrantedPermissionProfile;
use pretty_assertions::assert_eq;

fn bash_request() -> Value {
    json!({
        "subtype": "can_use_tool",
        "tool_name": "Bash",
        "input": {"command": "cargo test", "description": "Run the tests"},
        "tool_use_id": "toolu_1",
        "permission_suggestions": [{
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "cargo test:*"}],
            "behavior": "allow",
            "destination": "localSettings",
        }],
    })
}

fn pending(request: &Value) -> (ServerRequest, PendingApproval) {
    approval_request(
        RequestId::String("claude-approval-1".into()),
        "ctl-1",
        request,
        "thread-1",
        "turn-1",
        Path::new("/repo"),
        /*now_ms*/ 1_000,
    )
}

#[test]
fn bash_becomes_a_command_approval() {
    let (request, pending) = pending(&bash_request());
    let ServerRequest::CommandExecutionRequestApproval { request_id, params } = request else {
        panic!("expected a command approval, got {request:?}");
    };
    assert_eq!(request_id, RequestId::String("claude-approval-1".into()));
    assert_eq!(
        (
            params.thread_id.as_str(),
            params.turn_id.as_str(),
            params.item_id.as_str(),
            params.command.as_deref(),
            params.reason.as_deref(),
        ),
        (
            "thread-1",
            "turn-1",
            "toolu_1",
            Some("cargo test"),
            Some("Run the tests")
        )
    );
    assert_eq!(pending.kind, ApprovalKind::Command);
    assert_eq!(pending.control_request_id, "ctl-1");
}

#[test]
fn edit_tools_become_file_change_approvals_and_others_permissions() {
    let edit = json!({"subtype": "can_use_tool", "tool_name": "Write", "tool_use_id": "t2",
                      "input": {"file_path": "/repo/a.txt", "content": "x"}});
    assert!(matches!(
        pending(&edit).0,
        ServerRequest::FileChangeRequestApproval { .. }
    ));
    let fetch = json!({"subtype": "can_use_tool", "tool_name": "WebFetch", "tool_use_id": "t3",
                       "input": {"url": "https://example.com", "prompt": "summarize"}});
    let (request, pending) = pending(&fetch);
    let ServerRequest::PermissionsRequestApproval { params, .. } = request else {
        panic!("expected a permissions approval");
    };
    assert_eq!(
        params.permissions.network.and_then(|n| n.enabled),
        Some(true)
    );
    assert_eq!(pending.kind, ApprovalKind::Permissions);
}

#[test]
fn decisions_map_like_t3_code() {
    let (_, pending) = pending(&bash_request());
    let answer = |decision: &str| decision_from_response(&pending, &json!({"decision": decision}));
    assert_eq!(
        answer("accept").permission_result("toolu_1"),
        json!({"behavior": "allow", "updatedInput": {"command": "cargo test", "description": "Run the tests"}, "toolUseID": "toolu_1"})
    );
    assert_eq!(
        answer("acceptForSession").permission_result("toolu_1")["updatedPermissions"],
        json!([{
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "cargo test:*"}],
            "behavior": "allow",
            "destination": "session",
        }])
    );
    assert_eq!(
        answer("decline").permission_result("toolu_1"),
        json!({"behavior": "deny", "message": DECLINED, "toolUseID": "toolu_1"})
    );
    assert_eq!(
        answer("cancel").permission_result("toolu_1"),
        json!({"behavior": "deny", "message": CANCELLED, "toolUseID": "toolu_1", "interrupt": true})
    );
}

#[test]
fn an_empty_permission_grant_denies() {
    let fetch = json!({"subtype": "can_use_tool", "tool_name": "WebFetch", "tool_use_id": "t3", "input": {}});
    let (_, pending) = pending(&fetch);
    let denied = serde_json::to_value(PermissionsRequestApprovalResponse {
        permissions: GrantedPermissionProfile::default(),
        scope: PermissionGrantScope::Turn,
        strict_auto_review: None,
    })
    .unwrap();
    assert!(decision_from_response(&pending, &denied).is_deny());
    let granted = json!({"permissions": {"network": {"enabled": true}}, "scope": "session"});
    let decision = decision_from_response(&pending, &granted);
    assert_eq!(
        decision.permission_result("t3")["updatedPermissions"],
        json!([{"type": "addRules", "rules": [{"toolName": "WebFetch"}], "behavior": "allow", "destination": "session"}])
    );
}

#[test]
fn ask_user_question_becomes_request_user_input_and_answers_return_as_input() {
    let ask = json!({"subtype": "can_use_tool", "tool_name": "AskUserQuestion", "tool_use_id": "t4",
        "input": {"questions": [{"question": "Which database?", "header": "DB", "multiSelect": false,
            "options": [{"label": "Postgres", "description": "Relational"}, {"label": "SQLite"}]}]}});
    let (request, pending) = pending(&ask);
    let ServerRequest::ToolRequestUserInput { params, .. } = request else {
        panic!("expected request_user_input");
    };
    assert_eq!(params.questions[0].id, "Which database?");
    assert_eq!(params.questions[0].options.as_ref().map(Vec::len), Some(2));
    let decision = decision_from_response(
        &pending,
        &json!({"answers": {"Which database?": {"answers": ["SQLite"]}}}),
    );
    assert_eq!(
        decision.permission_result("t4")["updatedInput"]["answers"],
        json!({"Which database?": "SQLite"})
    );
}
