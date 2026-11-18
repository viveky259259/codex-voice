//! Protocol-to-engine mapping for the voice-command backend.

use super::*;
use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::PatchChangeKind;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::ThreadItem;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;

pub(crate) fn thread(id: &str, status: ThreadStatus) -> Thread {
    Thread {
        originator: None,
        environments: None,
        id: id.to_string(),
        extra: None,
        session_id: id.to_string(),
        forked_from_id: None,
        attached_to_thread_id: None,
        parent_thread_id: None,
        preview: String::new(),
        ephemeral: false,
        section: None,
        section_entered_at: None,
        project_id: None,
        daybreak_enabled: None,
        history_mode: Default::default(),
        model_provider: "openai".to_string(),
        model: None,
        reasoning_effort: None,
        created_at: 0,
        updated_at: 1_700_000_000,
        recency_at: None,
        status,
        path: None,
        cwd: test_path_buf("/tmp/project").abs(),
        cli_version: "0.0.0".to_string(),
        source: SessionSource::Cli,
        can_accept_direct_input: None,
        thread_source: None,
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: None,
        turns: Vec::new(),
    }
}

pub(crate) fn turn(id: &str, status: TurnStatus, items: Vec<ThreadItemPayload>) -> Turn {
    Turn {
        id: id.to_string(),
        root_turn_id: None,
        items: items.into_iter().map(ThreadItem::untimed).collect(),
        items_view: TurnItemsView::Full,
        status,
        error: None,
        started_at: Some(1_700_000_100),
        completed_at: None,
        duration_ms: None,
    }
}

pub(crate) fn command(text: &str) -> ThreadItemPayload {
    serde_json::from_value(serde_json::json!({
        "type": "commandExecution",
        "id": "cmd-1",
        "command": text,
        "cwd": "/tmp/project",
        "processId": null,
        "source": "agent",
        "status": "inProgress",
        "commandActions": [],
        "aggregatedOutput": "running 3 tests",
        "exitCode": null,
        "durationMs": null
    }))
    .expect("command item")
}

#[test]
fn thread_status_maps_to_agent_status() {
    for (status, expected) in [
        (ThreadStatus::NotLoaded, AgentStatus::NotLoaded),
        (ThreadStatus::Idle, AgentStatus::Idle),
        (ThreadStatus::SystemError, AgentStatus::Error),
        (
            ThreadStatus::Active {
                active_flags: Vec::new(),
            },
            AgentStatus::Running,
        ),
        (
            ThreadStatus::Active {
                active_flags: vec![ThreadActiveFlag::WaitingOnUserInput],
            },
            AgentStatus::WaitingForInput,
        ),
        (
            ThreadStatus::Active {
                active_flags: vec![
                    ThreadActiveFlag::WaitingOnUserInput,
                    ThreadActiveFlag::WaitingOnApproval,
                ],
            },
            AgentStatus::WaitingForApproval,
        ),
    ] {
        assert_eq!(agent_status(&status), expected, "{status:?}");
    }
}

#[test]
fn agent_names_fall_back_from_title_to_nickname_to_preview_to_id() {
    let mut named = thread("0199-abcd", ThreadStatus::Idle);
    named.preview = "Fix the flaky parser test in core please".to_string();
    assert_eq!(
        agent_name(&named),
        "Fix the flaky parser test in core please"
    );
    // A Claude Code session has no generated title: its first message names it,
    // cut at a word, not mid-phrase.
    named.preview = "List the files in this folder".to_string();
    assert_eq!(agent_name(&named), "List the files in this folder");
    named.preview = "List the files. Then count them.".to_string();
    assert_eq!(agent_name(&named), "List the files");
    named.preview = "Refactor the parser so empty input no longer panics".to_string();
    assert_eq!(agent_name(&named), "Refactor the parser so empty input no…");
    named.preview = "Fix the flaky parser test in core please".to_string();
    named.agent_nickname = Some("Scout".to_string());
    assert_eq!(agent_name(&named), "Scout");
    named.name = Some("  build  ".to_string());
    assert_eq!(agent_name(&named), "build");
    named.name = Some(" ".to_string());
    assert_eq!(agent_name(&named), "Scout");

    let anonymous = thread("0199-abcd", ThreadStatus::Idle);
    assert_eq!(agent_name(&anonymous), "agent abcd");
}

#[test]
fn an_active_thread_reports_its_running_turn() {
    let mut build = thread(
        "t-build",
        ThreadStatus::Active {
            active_flags: Vec::new(),
        },
    );
    build.name = Some("build".to_string());
    let running = turn(
        "turn-1",
        TurnStatus::InProgress,
        vec![command("cargo test")],
    );

    assert_eq!(
        agent_info(&build, Some(&running)),
        AgentInfo {
            id: "t-build".to_string(),
            name: "build".to_string(),
            status: AgentStatus::Running,
            cwd: Some(test_path_buf("/tmp/project")),
            preview: String::new(),
            updated_at: Some(1_700_000_000),
            turn_started_at: Some(1_700_000_100),
            latest: Some(ActivityItem::Command {
                command: "cargo test".to_string(),
                status: ItemStatus::InProgress,
                exit_code: None,
                output: Some("running 3 tests".to_string()),
            }),
        }
    );

    let finished = turn("turn-0", TurnStatus::Completed, Vec::new());
    assert_eq!(agent_info(&build, Some(&finished)).turn_started_at, None);
}

#[test]
fn items_keep_only_what_is_worth_saying() {
    let file_change = ThreadItemPayload::FileChange {
        id: "patch-1".to_string(),
        changes: vec![FileUpdateChange {
            path: "src/lib.rs".to_string(),
            kind: PatchChangeKind::Add,
            diff: "PRIVATE DIFF".to_string(),
        }],
        status: PatchApplyStatus::Completed,
    };
    assert_eq!(
        activity_item(&file_change),
        ActivityItem::FileChange {
            paths: vec!["src/lib.rs".to_string()],
            status: ItemStatus::Completed,
        }
    );
    let user = ThreadItemPayload::UserMessage {
        id: "user-1".to_string(),
        client_id: None,
        content: vec![UserInput::Text {
            text: "run the tests".to_string(),
            text_elements: Vec::new(),
        }],
    };
    assert_eq!(
        activity_item(&user),
        ActivityItem::UserMessage {
            text: "run the tests".to_string()
        }
    );
    let plan = ThreadItemPayload::Plan {
        id: "plan-1".to_string(),
        text: "1. test".to_string(),
    };
    assert_eq!(
        activity_item(&plan),
        ActivityItem::Plan {
            text: "1. test".to_string()
        }
    );
}

#[test]
fn request_errors_map_to_backend_errors() {
    let server = |message: &str| TypedRequestError::Server {
        method: "thread/read".to_string(),
        source: JSONRPCErrorError {
            code: -32600,
            message: message.to_string(),
            data: None,
        },
    };
    assert_eq!(
        backend_error("t-1", server("thread not found: t-1")),
        BackendError::AgentNotFound("t-1".to_string())
    );
    assert_eq!(
        backend_error("t-1", server("invalid thread id: bad")),
        BackendError::AgentNotFound("t-1".to_string())
    );
    assert!(matches!(
        backend_error("t-1", server("turn mismatch")),
        BackendError::Request(_)
    ));
    assert!(matches!(
        backend_error(
            "t-1",
            TypedRequestError::Transport {
                method: "thread/read".to_string(),
                source: std::io::Error::other("closed"),
            }
        ),
        BackendError::Unreachable(_)
    ));
}

#[test]
fn request_params_target_the_newest_turn() {
    assert_eq!(
        newest_turn_params("t-1", /*with_items*/ true),
        ThreadTurnsListParams {
            thread_id: "t-1".to_string(),
            cursor: None,
            limit: Some(1),
            sort_direction: Some(SortDirection::Desc),
            items_view: Some(TurnItemsView::Full),
        }
    );
    let input = vec![UserInput::Text {
        text: "run the tests".to_string(),
        text_elements: Vec::new(),
    }];
    let steer = steer_params("t-1", "turn-1".to_string(), input.clone());
    assert_eq!(steer.expected_turn_id, "turn-1");
    assert_eq!(steer.input, input);
    assert!(steer.client_user_message_id.is_some());
    let start = start_params("t-1", input.clone());
    assert_eq!(start.thread_id, "t-1");
    assert_eq!(start.input, input);
    assert_eq!(start.model, None);
}

#[test]
fn models_are_spoken_by_slug_and_display_name() {
    let mut opus = crate::test_support::TEST_MODEL_PRESETS
        .first()
        .cloned()
        .expect("a preset");
    opus.model = "claude-opus-5-5".to_string();
    opus.display_name = "Claude Opus".to_string();
    let mut plain = opus.clone();
    plain.model = "gpt-5.5".to_string();
    plain.display_name = "gpt-5.5".to_string();

    assert_eq!(
        voice_models(&[opus.clone(), plain, opus]),
        vec![
            ModelRef {
                slug: "claude-opus-5-5".to_string(),
                spoken_aliases: vec!["Claude Opus".to_string()],
            },
            ModelRef {
                slug: "gpt-5.5".to_string(),
                spoken_aliases: Vec::new(),
            },
        ]
    );
}

#[test]
fn machine_names_drop_the_domain() {
    assert_eq!(short_host_name("studio.local"), "studio");
    assert_eq!(short_host_name("desktop"), "desktop");
    assert_eq!(short_host_name(""), "this machine");
}

/// An in-memory app server: build is running a turn, docs is idle. Records
/// every request as (method, params). `older_server` rejects
/// `thread/turns/list`, like servers that predate it.
struct FakeTransport {
    link: MachineLink,
    failure: Option<BackendError>,
    older_server: bool,
    calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

impl FakeTransport {
    fn new() -> Self {
        Self {
            link: MachineLink::Connected,
            failure: None,
            older_server: false,
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Methods called, sorted: per-thread reads run concurrently.
    fn methods(&self) -> Vec<String> {
        let mut methods: Vec<String> = self
            .calls
            .lock()
            .expect("calls")
            .iter()
            .map(|(method, _)| method.clone())
            .collect();
        methods.sort();
        methods
    }

    fn params(&self, method: &str) -> Vec<serde_json::Value> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    fn reply(&self, method: &str, params: &serde_json::Value) -> RequestResult {
        let thread_id = params["threadId"].as_str().unwrap_or_default();
        let found = |include_turns: bool| -> Option<Thread> {
            let mut found = match thread_id {
                "t-build" => thread(
                    "t-build",
                    ThreadStatus::Active {
                        active_flags: Vec::new(),
                    },
                ),
                "t-docs" => thread("t-docs", ThreadStatus::Idle),
                _ => return None,
            };
            found.name = Some(thread_id.trim_start_matches("t-").to_string());
            if include_turns {
                found.turns = self.turns(thread_id);
            }
            Some(found)
        };
        let not_found = || JSONRPCErrorError {
            code: -32600,
            message: format!("thread not found: {thread_id}"),
            data: None,
        };
        let value = match method {
            "thread/loaded/list" => serde_json::json!({"data": ["t-build", "t-docs"]}),
            "thread/read" => {
                let include_turns = params["includeTurns"].as_bool().unwrap_or(false);
                let thread = found(include_turns).ok_or_else(not_found)?;
                serde_json::to_value(ThreadReadResponse {
                    thread,
                    read_state: None,
                })
                .expect("thread/read")
            }
            "thread/turns/list" if self.older_server => {
                return Err(JSONRPCErrorError {
                    code: -32601,
                    message: "method not found".to_string(),
                    data: None,
                });
            }
            "thread/turns/list" => serde_json::to_value(ThreadTurnsListResponse {
                data: self.turns(thread_id),
                next_cursor: None,
                backwards_cursor: None,
            })
            .expect("thread/turns/list"),
            "turn/steer" => serde_json::json!({"turnId": params["expectedTurnId"]}),
            "turn/start" => serde_json::to_value(TurnStartResponse {
                turn: turn("turn-new", TurnStatus::InProgress, Vec::new()),
            })
            .expect("turn/start"),
            "turn/interrupt" => serde_json::json!({}),
            other => panic!("unexpected request {other}"),
        };
        Ok(value)
    }

    fn turns(&self, thread_id: &str) -> Vec<Turn> {
        match thread_id {
            "t-build" => vec![turn(
                "turn-build",
                TurnStatus::InProgress,
                vec![command("cargo test")],
            )],
            _ => vec![turn("turn-docs", TurnStatus::Completed, Vec::new())],
        }
    }
}

impl AppServerTransport for FakeTransport {
    fn send(&self, request: ClientRequest) -> BoxFuture<'_, Result<RequestResult, BackendError>> {
        let request = serde_json::to_value(&request).expect("request serializes");
        let method = request["method"].as_str().unwrap_or_default().to_string();
        let params = request["params"].clone();
        async move {
            if let Some(failure) = self.failure.clone() {
                return Err(failure);
            }
            self.calls
                .lock()
                .expect("calls")
                .push((method.clone(), params.clone()));
            Ok(self.reply(&method, &params))
        }
        .boxed()
    }

    fn link(&self) -> MachineLink {
        self.link
    }
}

#[tokio::test]
async fn the_mapping_lists_agents_over_any_transport() {
    let backend = AppServerAgentBackend::new("devbox".to_string(), FakeTransport::new());
    assert_eq!(backend.machine(), "devbox");
    assert_eq!(backend.link(), MachineLink::Connected);

    let agents = backend.list_agents().await.expect("list");
    assert_eq!(
        agents
            .iter()
            .map(|agent| (agent.name.as_str(), agent.status))
            .collect::<Vec<_>>(),
        vec![("build", AgentStatus::Running), ("docs", AgentStatus::Idle)]
    );
    assert_eq!(agents[0].turn_started_at, Some(1_700_000_100));
    assert_eq!(
        backend.transport.methods(),
        vec![
            "thread/loaded/list",
            "thread/read",
            "thread/read",
            "thread/turns/list",
        ]
    );
    assert_eq!(
        backend.transport.params("thread/turns/list"),
        vec![serde_json::to_value(newest_turn_params("t-build", /*with_items*/ true)).unwrap()]
    );

    let report = backend.agent_activity("t-build").await.expect("activity");
    assert_eq!(report.agent.name, "build");
    assert_eq!(
        report.recent,
        vec![ActivityItem::Command {
            command: "cargo test".to_string(),
            status: ItemStatus::InProgress,
            exit_code: None,
            output: Some("running 3 tests".to_string()),
        }]
    );
    assert_eq!(
        backend.agent_activity("t-gone").await,
        Err(BackendError::AgentNotFound("t-gone".to_string()))
    );
}

#[tokio::test]
async fn the_mapping_steers_starts_and_interrupts_over_any_transport() {
    let backend = AppServerAgentBackend::new("devbox".to_string(), FakeTransport::new());

    assert_eq!(
        backend.steer("t-build", "run the tests").await,
        Ok(SteerOutcome::Steered {
            turn_id: "turn-build".to_string()
        })
    );
    let steer = backend.transport.params("turn/steer");
    assert_eq!(steer.len(), 1);
    assert_eq!(steer[0]["threadId"], "t-build");
    assert_eq!(steer[0]["expectedTurnId"], "turn-build");
    assert_eq!(steer[0]["input"][0]["text"], "run the tests");

    assert_eq!(
        backend.steer("t-docs", "update the readme").await,
        Ok(SteerOutcome::Started {
            turn_id: "turn-new".to_string()
        })
    );
    let start = backend.transport.params("turn/start");
    assert_eq!(start.len(), 1);
    assert_eq!(start[0]["threadId"], "t-docs");
    assert_eq!(start[0]["input"][0]["text"], "update the readme");

    assert_eq!(
        backend.interrupt("t-docs").await,
        Ok(InterruptOutcome::AlreadyIdle)
    );
    assert_eq!(
        backend.transport.params("turn/interrupt"),
        Vec::<serde_json::Value>::new()
    );
    assert_eq!(
        backend.interrupt("t-build").await,
        Ok(InterruptOutcome::Interrupted {
            turn_id: "turn-build".to_string()
        })
    );
    assert_eq!(
        backend.transport.params("turn/interrupt"),
        vec![serde_json::json!({"threadId": "t-build", "turnId": "turn-build"})]
    );
}

#[tokio::test]
async fn older_servers_fall_back_to_reading_the_whole_thread() {
    let transport = FakeTransport {
        older_server: true,
        ..FakeTransport::new()
    };
    let backend = AppServerAgentBackend::new("devbox".to_string(), transport);
    let report = backend.agent_activity("t-build").await.expect("activity");
    assert_eq!(report.recent.len(), 1);
    assert_eq!(
        backend
            .transport
            .params("thread/read")
            .iter()
            .map(|params| params["includeTurns"].as_bool().unwrap_or(false))
            .collect::<Vec<_>>(),
        vec![false, true]
    );
}

#[tokio::test]
async fn transport_failures_reach_the_orchestrator_unchanged() {
    for failure in [
        BackendError::Connecting,
        BackendError::Unreachable("ssh: connect to host devbox port 22".to_string()),
    ] {
        let transport = FakeTransport {
            link: MachineLink::Connecting,
            failure: Some(failure.clone()),
            ..FakeTransport::new()
        };
        let backend = AppServerAgentBackend::new("devbox".to_string(), transport);
        assert_eq!(backend.link(), MachineLink::Connecting);
        assert_eq!(backend.list_agents().await, Err(failure.clone()));
        assert_eq!(
            backend.steer("t-build", "run the tests").await,
            Err(failure.clone())
        );
        assert_eq!(backend.interrupt("t-build").await, Err(failure));
        assert_eq!(backend.transport.methods(), Vec::<String>::new());
    }
}

#[test]
fn results_decode_like_typed_requests() {
    let decoded: Result<TurnInterruptResponse, _> =
        decode_result("turn/interrupt", Ok(serde_json::json!({})));
    assert!(decoded.is_ok());
    let error = decode_result::<ThreadReadResponse>(
        "thread/read",
        Err(JSONRPCErrorError {
            code: -32600,
            message: "thread not found: t-1".to_string(),
            data: None,
        }),
    )
    .expect_err("server error");
    assert_eq!(
        backend_error("t-1", error),
        BackendError::AgentNotFound("t-1".to_string())
    );
    let malformed = decode_result::<ThreadReadResponse>("thread/read", Ok(serde_json::json!(7)))
        .expect_err("decode error");
    assert!(matches!(
        backend_error("t-1", malformed),
        BackendError::Request(_)
    ));
}
