//! Drives the backend through `AppServerClient` against a fake `claude`
//! (tests/fake_claude.py, run with `python3 -I`). The real CLI is never run.

use codex_app_server_client::AppServerClient;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ModelListParams;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_claude_agent_backend::ClaudeCodeOptions;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

struct Fake {
    dir: TempDir,
    client: AppServerClient,
    next_id: i64,
}

impl Fake {
    /// None when python3 is missing (the test is then skipped).
    fn start(scenario: &str, model: Option<&str>) -> Option<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = codex_claude_agent_backend::test_support::write_fake_claude(dir.path())?;
        let mut options = ClaudeCodeOptions::new(dir.path().to_path_buf());
        options.claude_bin = script;
        options.model = model.map(str::to_string);
        options.env = vec![
            (
                "FAKE_CLAUDE_LOG".into(),
                dir.path().join("log.jsonl").into(),
            ),
            ("FAKE_CLAUDE_SCENARIO".into(), scenario.into()),
        ];
        options.init_timeout = Duration::from_secs(10);
        let client = codex_claude_agent_backend::start(options);
        Some(Self {
            dir,
            client,
            next_id: 0,
        })
    }

    fn id(&mut self) -> RequestId {
        self.next_id += 1;
        RequestId::Integer(self.next_id)
    }

    async fn thread_start(&mut self) -> ThreadStartResponse {
        let request_id = self.id();
        self.client
            .request_typed(ClientRequest::ThreadStart {
                request_id,
                params: ThreadStartParams::default(),
            })
            .await
            .expect("thread/start")
    }

    async fn turn_start(&mut self, thread_id: &str, text: &str) -> TurnStartResponse {
        let request_id = self.id();
        self.client
            .request_typed(ClientRequest::TurnStart {
                request_id,
                params: TurnStartParams {
                    thread_id: thread_id.to_string(),
                    client_user_message_id: Some("client-msg-1".into()),
                    input: vec![UserInput::Text {
                        text: text.to_string(),
                        text_elements: Vec::new(),
                    }],
                    ..Default::default()
                },
            })
            .await
            .expect("turn/start")
    }

    async fn next_event(&mut self) -> AppServerEvent {
        tokio::time::timeout(Duration::from_secs(10), self.client.next_event())
            .await
            .expect("an event within 10s")
            .expect("the backend is open")
    }

    /// Events up to and including the first one `stop` accepts.
    async fn events_until(
        &mut self,
        stop: impl Fn(&AppServerEvent) -> bool,
    ) -> Vec<AppServerEvent> {
        let mut events = Vec::new();
        loop {
            let event = self.next_event().await;
            let done = stop(&event);
            events.push(event);
            if done {
                return events;
            }
        }
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("log.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("log line"))
            .collect()
    }

    fn stdin(&self) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter_map(|entry| entry.get("stdin").cloned())
            .collect()
    }

    fn argvs(&self) -> Vec<Vec<String>> {
        self.log()
            .into_iter()
            .filter_map(|entry| serde_json::from_value(entry.get("argv")?.clone()).ok())
            .collect()
    }

    /// Waits until the fake has logged a stdin message matching `want`.
    async fn wait_for_stdin(&self, want: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..200 {
            if let Some(found) = self.stdin().into_iter().find(|message| want(message)) {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "the fake CLI never received the expected message: {:#?}",
            self.stdin()
        );
    }
}

fn is_turn_completed(event: &AppServerEvent) -> bool {
    matches!(event, AppServerEvent::ServerNotification(n) if matches!(**n, ServerNotification::TurnCompleted(_)))
}

fn is_server_request(event: &AppServerEvent) -> bool {
    matches!(event, AppServerEvent::ServerRequest(_))
}

/// A compact, exact description of each event.
fn describe(events: &[AppServerEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            AppServerEvent::ServerNotification(notification) => match notification.as_ref() {
                ServerNotification::ThreadStatusChanged(n) => {
                    format!("thread/status/changed {:?}", n.status)
                }
                ServerNotification::TurnStarted(_) => "turn/started".to_string(),
                ServerNotification::TurnCompleted(n) => format!(
                    "turn/completed {:?} {:?}",
                    n.turn.status,
                    n.turn.error.as_ref().map(|error| error.message.as_str())
                ),
                ServerNotification::ItemStarted(n) => {
                    format!("item/started {}", item(&n.item.payload))
                }
                ServerNotification::ItemCompleted(n) => {
                    format!("item/completed {}", item(&n.item.payload))
                }
                ServerNotification::AgentMessageDelta(n) => {
                    format!("item/agentMessage/delta {:?}", n.delta)
                }
                ServerNotification::ReasoningSummaryTextDelta(n) => {
                    format!("item/reasoning/summaryTextDelta {:?}", n.delta)
                }
                ServerNotification::ThreadTokenUsageUpdated(n) => format!(
                    "thread/tokenUsage/updated total={} output={}",
                    n.token_usage.last.total_tokens, n.token_usage.last.output_tokens
                ),
                ServerNotification::ServerRequestResolved(n) => {
                    format!("serverRequest/resolved {:?}", n.request_id)
                }
                other => format!("{other:?}"),
            },
            AppServerEvent::ServerRequest(request) => match request.as_ref() {
                ServerRequest::CommandExecutionRequestApproval { params, .. } => format!(
                    "item/commandExecution/requestApproval {} {:?}",
                    params.item_id, params.command
                ),
                ServerRequest::FileChangeRequestApproval { params, .. } => {
                    format!("item/fileChange/requestApproval {}", params.item_id)
                }
                other => format!("{other:?}"),
            },
            other => format!("{other:?}"),
        })
        .collect()
}

fn item(payload: &ThreadItemPayload) -> String {
    match payload {
        ThreadItemPayload::UserMessage { client_id, .. } => format!("userMessage {client_id:?}"),
        ThreadItemPayload::AgentMessage { text, .. } => format!("agentMessage {text:?}"),
        ThreadItemPayload::Reasoning { summary, .. } => format!("reasoning {summary:?}"),
        ThreadItemPayload::CommandExecution {
            command,
            status,
            aggregated_output,
            ..
        } => format!("commandExecution {command:?} {status:?} {aggregated_output:?}"),
        ThreadItemPayload::FileChange {
            changes, status, ..
        } => format!(
            "fileChange {:?} {status:?}",
            changes
                .iter()
                .map(|change| Path::new(&change.path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned())
                .collect::<Vec<_>>()
        ),
        other => format!("{other:?}"),
    }
}

fn request_id_of(event: &AppServerEvent) -> RequestId {
    match event {
        AppServerEvent::ServerRequest(request) => request.id().clone(),
        other => panic!("not a server request: {other:?}"),
    }
}

#[tokio::test]
async fn a_text_and_thinking_reply() {
    let Some(mut fake) = Fake::start("text", /*model*/ None) else {
        eprintln!("python3 not found; skipping");
        return;
    };
    let started = fake.thread_start().await;
    assert!(uuid::Uuid::parse_str(&started.thread.id).is_ok());
    assert_eq!(started.model_provider, "claude-code");
    let events = fake.events_until(|event| matches!(event, AppServerEvent::ServerNotification(n) if matches!(**n, ServerNotification::ThreadStarted(_)))).await;
    assert_eq!(events.len(), 1);

    let turn = fake.turn_start(&started.thread.id, "Say hello").await;
    assert_eq!(turn.turn.status, TurnStatus::InProgress);
    let events = fake.events_until(is_turn_completed).await;
    assert_eq!(
        describe(&events),
        vec![
            "thread/status/changed Active { active_flags: [] }",
            "turn/started",
            "item/started userMessage Some(\"client-msg-1\")",
            "item/completed userMessage Some(\"client-msg-1\")",
            "item/started reasoning []",
            "item/reasoning/summaryTextDelta \"Thinking it over.\"",
            "item/completed reasoning [\"Thinking it over.\"]",
            "item/started agentMessage \"\"",
            "item/agentMessage/delta \"Hello\"",
            "item/agentMessage/delta \" from Claude.\"",
            "item/completed agentMessage \"Hello from Claude.\"",
            "thread/tokenUsage/updated total=115 output=5",
            "turn/completed Completed None",
        ]
    );
    let argv = &fake.argvs()[0];
    assert!(argv.contains(&format!("--session-id={}", started.thread.id)));
    assert!(!argv.iter().any(|arg| arg.starts_with("--model")));
    let stdin = fake.stdin();
    assert_eq!(
        stdin[0],
        json!({"type": "control_request", "request_id": "init-1", "request": {"subtype": "initialize"}})
    );
    assert_eq!(
        stdin[1],
        json!({"type": "user", "session_id": "", "parent_tool_use_id": null,
               "message": {"role": "user", "content": [{"type": "text", "text": "Say hello"}]}})
    );
}

#[tokio::test]
async fn bash_is_approved_through_the_approval_request() {
    let Some(mut fake) = Fake::start("bash", /*model*/ None) else {
        return;
    };
    let started = fake.thread_start().await;
    fake.events_until(|_| true).await;
    fake.turn_start(&started.thread.id, "Run echo").await;

    let events = fake.events_until(is_server_request).await;
    assert_eq!(
        describe(&events),
        vec![
            "thread/status/changed Active { active_flags: [] }",
            "turn/started",
            "item/started userMessage Some(\"client-msg-1\")",
            "item/completed userMessage Some(\"client-msg-1\")",
            "item/started commandExecution \"echo hi\" InProgress None",
            "item/commandExecution/requestApproval toolu_bash Some(\"echo hi\")",
        ]
    );
    let request_id = request_id_of(events.last().expect("request"));
    fake.client
        .resolve_server_request(request_id.clone(), json!({"decision": "accept"}))
        .await
        .expect("resolve");

    let events = fake.events_until(is_turn_completed).await;
    assert_eq!(
        describe(&events),
        vec![
            format!("serverRequest/resolved {request_id:?}"),
            "item/completed commandExecution \"echo hi\" Completed Some(\"hi\")".to_string(),
            "item/started agentMessage \"\"".to_string(),
            "item/agentMessage/delta \"Done.\"".to_string(),
            "item/completed agentMessage \"Done.\"".to_string(),
            "thread/tokenUsage/updated total=115 output=5".to_string(),
            "turn/completed Completed None".to_string(),
        ]
    );
    let answer = fake
        .wait_for_stdin(|message| message["type"] == "control_response")
        .await;
    assert_eq!(
        answer,
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "perm-1",
                "response": {
                    "behavior": "allow",
                    "updatedInput": {"command": "echo hi", "description": "Print hi"},
                    "toolUseID": "toolu_bash",
                },
            },
        })
    );
}

#[tokio::test]
async fn a_denied_edit_is_declined() {
    let Some(mut fake) = Fake::start("edit", /*model*/ None) else {
        return;
    };
    let started = fake.thread_start().await;
    fake.events_until(|_| true).await;
    fake.turn_start(&started.thread.id, "Write notes").await;
    let events = fake.events_until(is_server_request).await;
    assert_eq!(
        describe(&events[4..]),
        vec![
            "item/started fileChange [\"notes.md\"] InProgress",
            "item/fileChange/requestApproval toolu_write",
        ]
    );
    let ThreadItemPayload::FileChange { changes, .. } = (match &events[4] {
        AppServerEvent::ServerNotification(n) => match n.as_ref() {
            ServerNotification::ItemStarted(n) => n.item.payload.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }) else {
        panic!("not a file change");
    };
    assert_eq!(changes[0].diff, "hello\n");

    let request_id = request_id_of(events.last().expect("request"));
    fake.client
        .resolve_server_request(request_id, json!({"decision": "decline"}))
        .await
        .expect("resolve");
    let events = fake.events_until(is_turn_completed).await;
    assert!(
        describe(&events)
            .contains(&"item/completed fileChange [\"notes.md\"] Declined".to_string()),
        "{:#?}",
        describe(&events)
    );
    let answer = fake
        .wait_for_stdin(|message| message["type"] == "control_response")
        .await;
    assert_eq!(
        answer["response"]["response"],
        json!({"behavior": "deny", "message": "User declined tool execution.", "toolUseID": "toolu_write"})
    );
    assert!(!fake.dir.path().join("notes.md").exists());
}

#[tokio::test]
async fn interrupt_mid_turn() {
    let Some(mut fake) = Fake::start("interrupt", /*model*/ None) else {
        return;
    };
    let started = fake.thread_start().await;
    fake.events_until(|_| true).await;
    let turn = fake
        .turn_start(&started.thread.id, "Work for a while")
        .await;
    fake.events_until(|event| {
        matches!(event, AppServerEvent::ServerNotification(n) if matches!(**n, ServerNotification::AgentMessageDelta(_)))
    })
    .await;
    let request_id = fake.id();
    fake.client
        .request(ClientRequest::TurnInterrupt {
            request_id,
            params: TurnInterruptParams {
                thread_id: started.thread.id.clone(),
                turn_id: turn.turn.id.clone(),
            },
        })
        .await
        .expect("send")
        .expect("turn/interrupt");
    let events = fake.events_until(is_turn_completed).await;
    assert_eq!(
        describe(&events).last().map(String::as_str),
        Some("turn/completed Interrupted None")
    );
    assert!(describe(&events).contains(&"item/completed agentMessage \"Working\"".to_string()));
    let interrupt = fake
        .wait_for_stdin(|message| message["request"]["subtype"] == "interrupt")
        .await;
    assert_eq!(interrupt["type"], "control_request");
}

#[tokio::test]
async fn resume_passes_the_session_id_and_model() {
    let Some(mut fake) = Fake::start("text", Some("claude-opus-5-5")) else {
        return;
    };
    let session_id = "0199a213-81c0-7800-8aa1-bbab2a035a53";
    let request_id = fake.id();
    let resumed: ThreadResumeResponse = fake
        .client
        .request_typed(ClientRequest::ThreadResume {
            request_id,
            params: ThreadResumeParams {
                thread_id: session_id.to_string(),
                ..Default::default()
            },
        })
        .await
        .expect("thread/resume");
    assert_eq!(resumed.thread.id, session_id);
    assert_eq!(resumed.model, "claude-opus-5-5");
    fake.turn_start(session_id, "Continue").await;
    fake.events_until(is_turn_completed).await;
    let argv = &fake.argvs()[0];
    assert!(argv.contains(&format!("--resume={session_id}")), "{argv:?}");
    assert!(
        argv.contains(&"--model=claude-opus-5-5".to_string()),
        "{argv:?}"
    );
    assert!(!argv.iter().any(|arg| arg.starts_with("--session-id")));
}

#[tokio::test]
async fn models_merge_init_with_the_fallback_and_switch_with_set_model() {
    let Some(mut fake) = Fake::start("text", /*model*/ None) else {
        return;
    };
    let request_id = fake.id();
    let models: ModelListResponse = fake
        .client
        .request_typed(ClientRequest::ModelList {
            request_id,
            params: ModelListParams::default(),
        })
        .await
        .expect("model/list");
    let listed: Vec<(String, String, usize, bool)> = models
        .data
        .iter()
        .map(|model| {
            (
                model.model.clone(),
                model.display_name.clone(),
                model.supported_reasoning_efforts.len(),
                model.is_default,
            )
        })
        .collect();
    assert_eq!(
        listed,
        vec![
            ("default".into(), "Default (recommended)".into(), 0, true),
            ("opus".into(), "Opus".into(), 5, false),
            ("sonnet".into(), "Sonnet".into(), 0, false),
            (
                "claude-fable-5-2".into(),
                "Claude Fable 5.2".into(),
                5,
                false
            ),
            (
                "claude-haiku-5-5".into(),
                "Claude Haiku 5.5".into(),
                5,
                false
            ),
        ]
    );

    // The eager process that listed the models becomes the thread.
    let started = fake.thread_start().await;
    assert_eq!(fake.argvs().len(), 1);
    for (requested, sent) in [
        ("sonnet", json!("sonnet")),
        ("fable", json!("claude-fable-5-2")),
        ("default", Value::Null),
    ] {
        let request_id = fake.id();
        fake.client
            .request(ClientRequest::ThreadSettingsUpdate {
                request_id,
                params: ThreadSettingsUpdateParams {
                    thread_id: started.thread.id.clone(),
                    model: Some(requested.to_string()),
                    ..Default::default()
                },
            })
            .await
            .expect("send")
            .expect("thread/settings/update");
        let written = fake
            .wait_for_stdin(|message| {
                message["request"]["subtype"] == "set_model" && message["request"]["model"] == sent
            })
            .await;
        assert_eq!(written["type"], "control_request");
        assert_eq!(
            written["request"],
            json!({"subtype": "set_model", "model": sent})
        );
    }
    let request_id = fake.id();
    let refused = fake
        .client
        .request(ClientRequest::ThreadSettingsUpdate {
            request_id,
            params: ThreadSettingsUpdateParams {
                thread_id: started.thread.id.clone(),
                model: Some("gpt-5.1".to_string()),
                ..Default::default()
            },
        })
        .await
        .expect("send");
    assert!(refused.is_err());
    // Still one process: switching never restarts the session.
    assert_eq!(fake.argvs().len(), 1);
}

#[tokio::test]
async fn unsupported_requests_are_refused_explicitly() {
    let Some(mut fake) = Fake::start("text", /*model*/ None) else {
        return;
    };
    let request_id = fake.id();
    let refused = fake
        .client
        .request(ClientRequest::ThreadCompactStart {
            request_id,
            params: codex_app_server_protocol::ThreadCompactStartParams {
                thread_id: "x".into(),
            },
        })
        .await
        .expect("send")
        .expect_err("refused");
    assert_eq!(refused.code, -32601);
    assert!(
        refused.message.contains("Claude Code"),
        "{}",
        refused.message
    );
}
