//! Approvals from agents on other machines: the request conversion, routing
//! by (machine, request id), the app's prompt and answer flow, and one request
//! over a fake `ssh`.

use super::*;
use crate::app::tests::make_test_app_with_channels;
use crate::app_event::AppEvent;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::ThreadParamsMode;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::FileChangeRequestApprovalParams;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_config::config_toml::VoiceMachine;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

type TestResult = color_eyre::Result<()>;

fn command_params(
    thread_id: &str,
    item_id: &str,
    command: &str,
) -> CommandExecutionRequestApprovalParams {
    CommandExecutionRequestApprovalParams {
        kind: Default::default(),
        thread_id: thread_id.to_string(),
        turn_id: "turn-1".to_string(),
        item_id: item_id.to_string(),
        started_at_ms: 0,
        approval_id: None,
        environment_id: None,
        reason: None,
        network_approval_context: None,
        command: Some(command.to_string()),
        cwd: None,
        command_actions: None,
        additional_permissions: None,
        proposed_execpolicy_amendment: None,
        proposed_network_policy_amendments: None,
        available_decisions: None,
    }
}

fn command_request(
    request_id: i64,
    thread_id: &str,
    item_id: &str,
    command: &str,
) -> ServerRequest {
    ServerRequest::CommandExecutionRequestApproval {
        request_id: RequestId::Integer(request_id),
        params: command_params(thread_id, item_id, command),
    }
}

fn build() -> RemoteAgent {
    RemoteAgent {
        name: "build".to_string(),
        cwd: Some(PathBuf::from("/srv/app")),
    }
}

fn accept(id: &str) -> AppCommand {
    AppCommand::ExecApproval {
        id: id.to_string(),
        turn_id: None,
        decision: CommandExecutionApprovalDecision::Accept,
    }
}

/// A fake app server on a WebSocket: answers `initialize`, then reports every
/// other message the client sends (responses to server requests included).
async fn fake_connection() -> color_eyre::Result<(
    AppServerSession,
    mpsc::UnboundedReceiver<JSONRPCMessage>,
    JoinHandle<color_eyre::Result<()>>,
)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let (observed, receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        while let Some(frame) = socket.next().await {
            let Message::Text(text) = frame? else {
                continue;
            };
            match serde_json::from_str::<JSONRPCMessage>(&text)? {
                JSONRPCMessage::Request(request) if request.method == "initialize" => {
                    socket
                        .send(Message::Text(
                            json!({"id": request.id, "result": {"userAgent": "remote-approvals-test"}})
                                .to_string()
                                .into(),
                        ))
                        .await?;
                }
                JSONRPCMessage::Notification(_) => {}
                message => observed.send(message)?,
            }
        }
        Ok(())
    });
    let client = crate::connect_remote_app_server(endpoint).await?;
    Ok((
        AppServerSession::new(
            crate::backend_id::BackendId::new(),
            client,
            ThreadParamsMode::Remote,
        ),
        receiver,
        task,
    ))
}

async fn next_message(
    messages: &mut mpsc::UnboundedReceiver<JSONRPCMessage>,
) -> color_eyre::Result<Option<JSONRPCMessage>> {
    Ok(timeout(Duration::from_secs(10), messages.recv()).await?)
}

fn response(id: i64, decision: &str) -> Option<JSONRPCMessage> {
    Some(JSONRPCMessage::Response(JSONRPCResponse {
        id: RequestId::Integer(id),
        result: json!({ "decision": decision }),
    }))
}

#[test]
fn remote_requests_become_prompts_labeled_with_machine_and_agent() {
    let alias = ThreadId::new();
    let Some(ApprovalRequest::Exec(exec)) = remote_approval_request(
        "devbox",
        &build(),
        alias,
        &command_request(7, "t-build", "call-1", "cargo test -p codex-tui"),
    ) else {
        panic!("a command approval becomes an exec prompt");
    };
    assert_eq!(exec.thread_id, alias);
    assert_eq!(exec.thread_label.as_deref(), Some("devbox · build"));
    assert_eq!(exec.id, "call-1");
    assert_eq!(exec.command, vec!["cargo", "test", "-p", "codex-tui"]);
    assert!(
        exec.available_decisions
            .contains(&CommandExecutionApprovalDecision::Accept)
    );

    let patch = ServerRequest::FileChangeRequestApproval {
        request_id: RequestId::Integer(8),
        params: FileChangeRequestApprovalParams {
            thread_id: "t-build".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: "patch-1".to_string(),
            started_at_ms: 0,
            reason: Some("write src".to_string()),
            grant_root: None,
        },
    };
    let Some(ApprovalRequest::ApplyPatch(patch)) =
        remote_approval_request("devbox", &build(), alias, &patch)
    else {
        panic!("a file change approval becomes a patch prompt");
    };
    assert_eq!(
        (
            patch.thread_label.as_deref(),
            patch.id.as_str(),
            patch.cwd.as_path(),
            patch.changes.len()
        ),
        (
            Some("devbox · build"),
            "patch-1",
            std::path::Path::new("/srv/app"),
            0
        )
    );
    let prompt = ApprovalRequest::ApplyPatch(patch);
    assert_eq!(prompt.remote_origin(), Some(("devbox", "build")));

    // Anything but an approval is left for the machine's own clients.
    let tool_call = ServerRequest::DynamicToolCall {
        request_id: RequestId::Integer(9),
        params: DynamicToolCallParams {
            thread_id: "t-build".to_string(),
            turn_id: "turn-1".to_string(),
            call_id: "tool-1".to_string(),
            namespace: None,
            tool: "lookup".to_string(),
            arguments: json!({}),
        },
    };
    assert!(remote_approval_request("devbox", &build(), alias, &tool_call).is_none());
}

#[tokio::test]
async fn answers_are_routed_by_machine_and_request_id() -> TestResult {
    let (devbox, mut devbox_messages, _devbox_task) = fake_connection().await?;
    let (nas, mut nas_messages, _nas_task) = fake_connection().await?;
    let mut approvals = RemoteApprovals::default();
    // Both machines use the same thread id, item id and request id.
    let on_devbox = approvals
        .note_request(
            "devbox",
            1,
            &build(),
            command_request(7, "t-build", "call-1", "cargo test"),
            devbox
                .request_handle()
                .server_request_reply_handle(RequestId::Integer(7)),
        )
        .expect("a prompt for devbox");
    let on_nas = approvals
        .note_request(
            "nas",
            1,
            &build(),
            command_request(7, "t-build", "call-1", "make"),
            nas.request_handle()
                .server_request_reply_handle(RequestId::Integer(7)),
        )
        .expect("a prompt for nas");
    assert_ne!(on_devbox.thread_id(), on_nas.thread_id());
    assert_eq!(approvals.machine_of(on_nas.thread_id()), Some("nas"));
    // A replay of the same request on the same connection adds no prompt.
    assert!(
        approvals
            .note_request(
                "devbox",
                1,
                &build(),
                command_request(7, "t-build", "call-1", "cargo test"),
                devbox
                    .request_handle()
                    .server_request_reply_handle(RequestId::Integer(7)),
            )
            .is_none()
    );

    let answer = approvals
        .take_response(on_devbox.thread_id(), &accept("call-1"))
        .expect("devbox's request is pending");
    assert_eq!(
        (answer.machine.as_str(), &answer.request_id, &answer.result),
        (
            "devbox",
            &RequestId::Integer(7),
            &json!({"decision": "accept"})
        )
    );
    answer.reply.resolve(answer.result).await?;
    assert_eq!(
        next_message(&mut devbox_messages).await?,
        response(7, "accept")
    );
    assert!(nas_messages.try_recv().is_err());

    // devbox's request is gone; nas's is still pending under its own key.
    assert!(
        approvals
            .take_response(on_devbox.thread_id(), &accept("call-1"))
            .is_none()
    );
    assert_eq!(
        approvals.resolve("devbox", "t-build", &RequestId::Integer(7)),
        None
    );
    assert_eq!(
        approvals.resolve("nas", "t-build", &RequestId::Integer(7)),
        Some(ResolvedAppServerRequest::ExecApproval {
            thread_id: on_nas.thread_id().to_string(),
            id: "call-1".to_string(),
        })
    );
    assert!(approvals.prompts().is_empty());
    // An answer to a local thread is not this registry's.
    assert!(
        approvals
            .take_response(ThreadId::new(), &accept("call-1"))
            .is_none()
    );
    Ok(())
}

fn voice_machines() -> Vec<VoiceMachine> {
    vec![VoiceMachine {
        host: "devbox".to_string(),
        name: None,
        codex_bin: None,
    }]
}

fn speech(ops: &mut mpsc::UnboundedReceiver<AppCommand>) -> Vec<String> {
    std::iter::from_fn(|| ops.try_recv().ok())
        .filter_map(|op| match op {
            AppCommand::RealtimeConversationSpeech { text, .. } => Some(text.as_str().to_string()),
            _ => None,
        })
        .collect()
}

fn submitted_ops(events: &mut mpsc::UnboundedReceiver<AppEvent>) -> Vec<(ThreadId, AppCommand)> {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::SubmitThreadOp { thread_id, op } => Some((thread_id, op)),
            _ => None,
        })
        .collect()
}

fn remote_request(
    remote: &AppServerSession,
    machine: &str,
    connection: u64,
    request: ServerRequest,
) -> RemoteApprovalEvent {
    RemoteApprovalEvent::Request {
        machine: machine.to_string(),
        connection,
        agent: build(),
        reply: remote
            .request_handle()
            .server_request_reply_handle(request.id().clone()),
        request,
    }
}

#[tokio::test]
async fn a_remote_approval_is_spoken_confirmed_and_answered_on_its_machine() -> TestResult {
    let (mut app, mut events, mut ops) = make_test_app_with_channels().await;
    app.config.realtime.voice_machines = voice_machines();
    crate::chatwidget::activate_voice_for_thread(&mut app.chat_widget, ThreadId::new());
    let (mut local, mut local_messages, _local_task) = fake_connection().await?;
    let (remote, mut remote_messages, _remote_task) = fake_connection().await?;

    app.handle_remote_approval_event(remote_request(
        &remote,
        "devbox",
        1,
        command_request(7, "t-build", "call-1", "cargo test"),
    ));
    let (alias, label) =
        crate::chatwidget::visible_approval_for_test(&app.chat_widget).expect("prompt shown");
    assert_eq!(label.as_deref(), Some("devbox · build"));
    assert_eq!(
        speech(&mut ops),
        vec!["Build on devbox wants to run cargo test. Say approve or deny.".to_string()]
    );

    // "approve" only arms it; nothing is sent anywhere.
    crate::chatwidget::say_for_test(&mut app.chat_widget, "approve");
    assert_eq!(submitted_ops(&mut events), Vec::new());
    assert_eq!(
        speech(&mut ops),
        vec!["Approve cargo test for build on devbox? Say confirm.".to_string()]
    );
    crate::chatwidget::say_for_test(&mut app.chat_widget, "confirm");
    let submitted = submitted_ops(&mut events);
    assert_eq!(submitted.len(), 1, "{submitted:?}");
    for (thread_id, op) in submitted {
        assert_eq!(thread_id, alias);
        app.submit_thread_op(&mut local, thread_id, op).await?;
    }
    assert_eq!(
        next_message(&mut remote_messages).await?,
        response(7, "accept")
    );

    // A denial applies at once, on the same machine.
    app.handle_remote_approval_event(remote_request(
        &remote,
        "devbox",
        1,
        command_request(8, "t-build", "call-2", "rm -rf target"),
    ));
    speech(&mut ops);
    crate::chatwidget::say_for_test(&mut app.chat_widget, "deny");
    for (thread_id, op) in submitted_ops(&mut events) {
        app.submit_thread_op(&mut local, thread_id, op).await?;
    }
    // The default command decisions offer no plain decline, so a spoken
    // denial cancels, as it does for this machine's prompts.
    assert_eq!(
        next_message(&mut remote_messages).await?,
        response(8, "cancel")
    );
    assert_eq!(speech(&mut ops), vec!["Denied.".to_string()]);
    assert!(crate::chatwidget::visible_approval_for_test(&app.chat_widget).is_none());

    // This machine's app server never saw either answer.
    assert!(
        timeout(Duration::from_millis(200), local_messages.recv())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn remote_prompts_close_when_answered_elsewhere_or_disconnected() -> TestResult {
    let (mut app, _events, mut ops) = make_test_app_with_channels().await;
    app.config.realtime.voice_machines = voice_machines();
    crate::chatwidget::activate_voice_for_thread(&mut app.chat_widget, ThreadId::new());
    let (remote, mut remote_messages, _remote_task) = fake_connection().await?;

    // Answered by someone at devbox.
    app.handle_remote_approval_event(remote_request(
        &remote,
        "devbox",
        1,
        command_request(7, "t-build", "call-1", "cargo test"),
    ));
    crate::chatwidget::say_for_test(&mut app.chat_widget, "approve");
    speech(&mut ops);
    app.handle_remote_approval_event(RemoteApprovalEvent::Resolved {
        machine: "devbox".to_string(),
        thread_id: "t-build".to_string(),
        request_id: RequestId::Integer(7),
    });
    assert!(crate::chatwidget::visible_approval_for_test(&app.chat_widget).is_none());

    // The turn ended on devbox: its prompt goes too.
    app.handle_remote_approval_event(remote_request(
        &remote,
        "devbox",
        1,
        command_request(8, "t-build", "call-2", "cargo build"),
    ));
    app.handle_remote_approval_event(RemoteApprovalEvent::TurnEnded {
        machine: "devbox".to_string(),
        thread_id: "t-build".to_string(),
        turn_id: "turn-1".to_string(),
    });
    assert!(crate::chatwidget::visible_approval_for_test(&app.chat_widget).is_none());
    speech(&mut ops);

    // The connection drops: an older connection's notice changes nothing,
    // the current one's closes the prompt and says so.
    app.handle_remote_approval_event(remote_request(
        &remote,
        "devbox",
        2,
        command_request(9, "t-build", "call-3", "cargo fmt"),
    ));
    speech(&mut ops);
    app.handle_remote_approval_event(RemoteApprovalEvent::Disconnected {
        machine: "devbox".to_string(),
        connection: 1,
    });
    assert!(crate::chatwidget::visible_approval_for_test(&app.chat_widget).is_some());
    app.handle_remote_approval_event(RemoteApprovalEvent::Disconnected {
        machine: "devbox".to_string(),
        connection: 2,
    });
    assert!(crate::chatwidget::visible_approval_for_test(&app.chat_widget).is_none());
    assert_eq!(
        speech(&mut ops),
        vec!["Lost devbox; its approval is still waiting there.".to_string()]
    );

    // Nothing was answered automatically.
    assert!(
        timeout(Duration::from_millis(200), remote_messages.recv())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn an_answer_that_cannot_reach_its_machine_says_so() -> TestResult {
    let (mut app, mut events, mut ops) = make_test_app_with_channels().await;
    app.config.realtime.voice_machines = voice_machines();
    crate::chatwidget::activate_voice_for_thread(&mut app.chat_widget, ThreadId::new());
    let (mut local, _local_messages, _local_task) = fake_connection().await?;
    let (remote, _remote_messages, remote_task) = fake_connection().await?;
    app.handle_remote_approval_event(remote_request(
        &remote,
        "devbox",
        1,
        command_request(7, "t-build", "call-1", "cargo test"),
    ));
    speech(&mut ops);
    remote.shutdown().await?;
    remote_task.await??;

    crate::chatwidget::say_for_test(&mut app.chat_widget, "deny");
    for (thread_id, op) in submitted_ops(&mut events) {
        app.submit_thread_op(&mut local, thread_id, op).await?;
    }
    assert_eq!(
        speech(&mut ops),
        vec![
            "Denied.".to_string(),
            "Couldn't reach devbox; the approval is still waiting there.".to_string()
        ]
    );
    Ok(())
}

#[tokio::test]
async fn requests_from_unlisted_machines_are_ignored() -> TestResult {
    let (mut app, _events, _ops) = make_test_app_with_channels().await;
    app.config.realtime.voice_machines = voice_machines();
    let (remote, _remote_messages, _remote_task) = fake_connection().await?;
    app.handle_remote_approval_event(remote_request(
        &remote,
        "nas",
        1,
        command_request(7, "t-build", "call-1", "cargo test"),
    ));
    assert!(crate::chatwidget::visible_approval_for_test(&app.chat_widget).is_none());
    assert!(app.voice_commands.remote_approvals.prompts().is_empty());
    Ok(())
}

/// The machine's app server behind a fake `ssh`: one idle agent, `build`.
/// After `thread/resume` it asks for approval of `cargo test` and reports
/// the answer it gets.
#[cfg(unix)]
async fn serve_devbox(
    stream: codex_uds::UnixStream,
    requests: mpsc::UnboundedSender<(String, serde_json::Value)>,
    answers: mpsc::UnboundedSender<JSONRPCMessage>,
) -> color_eyre::Result<()> {
    use crate::app::voice_command_backend::tests::thread;
    use codex_app_server_protocol::ThreadReadResponse;
    use codex_app_server_protocol::ThreadStatus;
    let mut socket = tokio_tungstenite::accept_async(stream).await?;
    while let Some(frame) = socket.next().await {
        let Message::Text(text) = frame? else {
            continue;
        };
        let request = match serde_json::from_str::<JSONRPCMessage>(&text)? {
            JSONRPCMessage::Request(request) => request,
            JSONRPCMessage::Notification(_) => continue,
            message => {
                answers.send(message)?;
                continue;
            }
        };
        let params = request.params.clone().unwrap_or_default();
        requests.send((request.method.clone(), params.clone()))?;
        let result = match request.method.as_str() {
            "initialize" => json!({"userAgent": "fake-devbox"}),
            "thread/loaded/list" => json!({"data": ["t-build"], "nextCursor": null}),
            "thread/read" => {
                let mut build = thread("t-build", ThreadStatus::Idle);
                build.name = Some("build".to_string());
                serde_json::to_value(ThreadReadResponse {
                    thread: build,
                    read_state: None,
                })?
            }
            // Its result is not read by the link.
            "thread/resume" => json!({}),
            other => panic!("unexpected request {other}"),
        };
        socket
            .send(Message::Text(
                json!({"id": request.id, "result": result})
                    .to_string()
                    .into(),
            ))
            .await?;
        if request.method == "thread/resume" {
            let approval = json!({
                "id": 7,
                "method": "item/commandExecution/requestApproval",
                "params": command_params("t-build", "call-1", "cargo test"),
            });
            socket
                .send(Message::Text(approval.to_string().into()))
                .await?;
        }
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn an_approval_from_a_machine_over_ssh_is_answered_over_ssh() -> TestResult {
    use crate::app::voice_command_remote::ApprovalForwarding;
    use crate::app::voice_command_remote::RemoteLink;
    use crate::app::voice_command_remote::tests::python3_available;
    use crate::app::voice_command_remote::tests::write_fake_ssh;
    use crate::app::voice_command_remote::tests::write_remote_codex;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    if !python3_available() {
        eprintln!("skipping: python3 is required for the stdio relay stub");
        return Ok(());
    }
    let dir = tempfile::TempDir::new()?;
    let socket_path = dir.path().join("control.sock");
    let fake_ssh = write_fake_ssh(
        dir.path(),
        &dir.path().join("ssh.log"),
        &dir.path().join("ssh.pids"),
    )?;
    let remote_codex = write_remote_codex(dir.path(), &socket_path)?;
    let mut listener = codex_uds::UnixListener::bind(&socket_path).await?;
    let (requests_tx, mut requests) = mpsc::unbounded_channel();
    let (answers_tx, mut answers) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(stream) = listener.accept().await {
            tokio::spawn(serve_devbox(
                stream,
                requests_tx.clone(),
                answers_tx.clone(),
            ));
        }
    });

    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    app.config.realtime.voice_machines = vec![VoiceMachine {
        host: "me@devbox:2222".to_string(),
        name: Some("devbox".to_string()),
        codex_bin: Some(remote_codex.display().to_string()),
    }];
    let mut target = crate::app::voice_command_remote::ssh_target(
        "me@devbox:2222",
        Some(&remote_codex.display().to_string()),
    )
    .map_err(|error| color_eyre::eyre::eyre!(error))?;
    target.ssh_program = Some(fake_ssh);
    let link = RemoteLink::new(target).with_approvals(ApprovalForwarding {
        machine: "devbox".to_string(),
        app_event_tx: app.app_event_tx.clone(),
    });
    link.start_connecting();

    // On connect the link lists the agents and subscribes; the request the
    // server then sends reaches the app.
    let event = loop {
        let event = timeout(Duration::from_secs(20), events.recv())
            .await?
            .expect("app events stay open");
        if let AppEvent::RemoteApproval(event) = event {
            break *event;
        }
    };
    app.handle_remote_approval_event(event);
    let (alias, label) =
        crate::chatwidget::visible_approval_for_test(&app.chat_widget).expect("prompt shown");
    assert_eq!(label.as_deref(), Some("devbox · build"));

    let mut seen = Vec::new();
    while let Ok((method, params)) = requests.try_recv() {
        seen.push((method, params));
    }
    let methods: Vec<&str> = seen.iter().map(|(method, _)| method.as_str()).collect();
    assert_eq!(
        methods,
        vec![
            "initialize",
            "thread/loaded/list",
            "thread/read",
            "thread/resume"
        ]
    );
    // A pure subscribe: no history, no setting overrides.
    let resume = seen[3].1.as_object().expect("thread/resume params");
    assert_eq!(resume.get("threadId"), Some(&json!("t-build")));
    assert_eq!(resume.get("excludeTurns"), Some(&json!(true)));
    let overrides: Vec<&String> = resume
        .iter()
        .filter(|(key, value)| {
            !matches!(key.as_str(), "threadId" | "excludeTurns") && !value.is_null()
        })
        .map(|(key, _)| key)
        .collect();
    assert_eq!(overrides, Vec::<&String>::new());

    // Approve with the keyboard; the answer goes back over ssh.
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    let (mut local, mut local_messages, _local_task) = fake_connection().await?;
    let submitted = submitted_ops(&mut events);
    assert_eq!(submitted.len(), 1, "{submitted:?}");
    for (thread_id, op) in submitted {
        assert_eq!(thread_id, alias);
        app.submit_thread_op(&mut local, thread_id, op).await?;
    }
    assert_eq!(
        timeout(Duration::from_secs(10), answers.recv()).await?,
        response(7, "accept")
    );
    assert!(
        timeout(Duration::from_millis(200), local_messages.recv())
            .await
            .is_err()
    );
    drop(link);
    Ok(())
}
