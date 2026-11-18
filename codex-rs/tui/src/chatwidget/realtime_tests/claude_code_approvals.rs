//! A Claude Code `can_use_tool` request, produced by the Claude Code backend
//! from a fake `claude` (python3 -I), shown in the approval overlay and answered
//! by voice. The answer goes back to the fake CLI as the SDK's PermissionResult.

use super::voice_approvals::drain_events;
use super::voice_approvals::exec_decision;
use super::voice_approvals::say;
use super::voice_approvals::spoken;
use super::*;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn a_claude_code_approval_is_shown_and_answered_by_voice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(script) = codex_claude_agent_backend::test_support::write_fake_claude(dir.path())
    else {
        eprintln!("python3 not found; skipping");
        return;
    };
    let log = dir.path().join("log.jsonl");
    let mut options = codex_claude_agent_backend::ClaudeCodeOptions::new(dir.path().to_path_buf());
    options.claude_bin = script;
    options.env = vec![
        ("FAKE_CLAUDE_LOG".into(), log.clone().into()),
        ("FAKE_CLAUDE_SCENARIO".into(), "bash".into()),
    ];
    let mut client = codex_claude_agent_backend::start(options);
    let started: ThreadStartResponse = client
        .request_typed(ClientRequest::ThreadStart {
            request_id: RequestId::Integer(1),
            params: Default::default(),
        })
        .await
        .expect("thread/start");
    let _: TurnStartResponse = client
        .request_typed(ClientRequest::TurnStart {
            request_id: RequestId::Integer(2),
            params: TurnStartParams {
                thread_id: started.thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "say hi".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await
        .expect("turn/start");
    let request: ServerRequest = loop {
        let event = tokio::time::timeout(Duration::from_secs(10), client.next_event())
            .await
            .expect("an approval request within 10s")
            .expect("backend open");
        if let AppServerEvent::ServerRequest(request) = event {
            break *request;
        }
    };
    let request_id = request.id().clone();

    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = ThreadId::from_string(&started.thread.id).expect("thread id");
    activate_voice_for_thread(&mut chat, thread_id);
    chat.handle_server_request(request, /*replay_kind*/ None);
    assert!(chat.bottom_pane.current_approval_request().is_some());
    assert_eq!(
        spoken(&mut ops),
        vec!["Codex wants to run echo hi. Say approve or deny.".to_string()]
    );

    say(&mut chat, "approve");
    assert_eq!(
        spoken(&mut ops),
        vec!["Approve echo hi? Say confirm.".to_string()]
    );
    say(&mut chat, "confirm");
    let (submitted, _) = drain_events(&mut chat, &mut events);
    let decisions: Vec<_> = submitted.iter().filter_map(exec_decision).collect();
    assert_eq!(
        decisions,
        vec![("toolu_bash", &CommandExecutionApprovalDecision::Accept)]
    );

    // The App answers the request it recorded with the overlay's decision.
    client
        .resolve_server_request(
            request_id,
            serde_json::to_value(CommandExecutionRequestApprovalResponse {
                decision: decisions[0].1.clone(),
            })
            .expect("serialize"),
        )
        .await
        .expect("resolve");
    let mut answer = None;
    for _ in 0..200 {
        answer = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|entry| entry.get("stdin").cloned())
            .find(|message| message["type"] == "control_response");
        if answer.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        answer.expect("the fake CLI received the answer")["response"],
        json!({
            "subtype": "success",
            "request_id": "perm-1",
            "response": {
                "behavior": "allow",
                "updatedInput": {"command": "echo hi", "description": "Print hi"},
                "toolUseID": "toolu_bash",
            },
        })
    );
    client.shutdown().await.expect("shutdown");
}
