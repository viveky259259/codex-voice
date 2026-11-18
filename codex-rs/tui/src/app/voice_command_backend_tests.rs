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
    assert_eq!(agent_name(&named), "Fix the flaky parser");
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
