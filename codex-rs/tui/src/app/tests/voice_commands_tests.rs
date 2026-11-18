//! Voice commands against a fake app server: the backend's RPCs, and the app
//! running a spoken command end to end through its orchestrator.

use super::disconnect::serve_reconnect_requests;
use super::*;
use crate::app::voice_command_backend::AppServerAgentBackend;
use crate::app::voice_command_backend::local_machine_name;
use crate::app::voice_command_backend::tests::command;
use crate::app::voice_command_backend::tests::thread;
use crate::app::voice_command_backend::tests::turn;
use crate::app::voice_commands::MachineDiscovery;
use crate::app_server_session::ThreadParamsMode;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::TurnStartResponse;
use codex_orchestrator::ActivityItem;
use codex_orchestrator::AgentBackend;
use codex_orchestrator::AgentStatus;
use codex_orchestrator::InterruptOutcome;
use codex_orchestrator::ItemStatus;
use codex_orchestrator::SteerOutcome;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Mutex as StdMutex;
use tokio::net::TcpListener;

type Recorded = Arc<StdMutex<Vec<(String, Value)>>>;

/// build is running a turn; docs is idle.
fn fake_reply(request: &JSONRPCRequest) -> Value {
    let params = request.params.clone().unwrap_or(Value::Null);
    let thread_id = params["threadId"].as_str().unwrap_or_default().to_string();
    let result = match request.method.as_str() {
        "thread/loaded/list" => json!({"data": ["t-build", "t-docs"], "nextCursor": null}),
        "thread/read" => {
            let mut found = match thread_id.as_str() {
                "t-build" => thread(
                    "t-build",
                    ThreadStatus::Active {
                        active_flags: Vec::new(),
                    },
                ),
                "t-docs" => thread("t-docs", ThreadStatus::Idle),
                _ => {
                    return json!({"error": {
                        "code": -32600,
                        "message": format!("thread not found: {thread_id}"),
                    }});
                }
            };
            found.name = Some(thread_id.trim_start_matches("t-").to_string());
            serde_json::to_value(ThreadReadResponse {
                thread: found,
                read_state: None,
            })
            .expect("thread/read")
        }
        "thread/turns/list" => {
            let data = match thread_id.as_str() {
                "t-build" => vec![turn(
                    "turn-build",
                    TurnStatus::InProgress,
                    vec![command("cargo test")],
                )],
                _ => vec![turn("turn-docs", TurnStatus::Completed, Vec::new())],
            };
            serde_json::to_value(ThreadTurnsListResponse {
                data,
                next_cursor: None,
                backwards_cursor: None,
            })
            .expect("thread/turns/list")
        }
        "turn/steer" => json!({"turnId": params["expectedTurnId"]}),
        "turn/start" => serde_json::to_value(TurnStartResponse {
            turn: turn("turn-new", TurnStatus::InProgress, Vec::new()),
        })
        .expect("turn/start"),
        "turn/interrupt" => json!({}),
        other => panic!("unexpected request {other}"),
    };
    json!({ "result": result })
}

async fn fake_app_server() -> Result<(AppServerSession, Recorded)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let recorded = Recorded::default();
    let sink = Arc::clone(&recorded);
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let socket = tokio_tungstenite::accept_async(stream).await?;
        serve_reconnect_requests(socket, move |request| {
            sink.lock().expect("recorder").push((
                request.method.clone(),
                request.params.clone().unwrap_or(Value::Null),
            ));
            std::future::ready(Some(fake_reply(&request)))
        })
        .await
    });
    let session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Embedded,
    );
    Ok((session, recorded))
}

fn methods(recorded: &Recorded) -> Vec<String> {
    let mut methods: Vec<String> = recorded
        .lock()
        .expect("recorder")
        .drain(..)
        .map(|(method, _)| method)
        .collect();
    methods.sort();
    methods
}

fn params(recorded: &Recorded, method: &str) -> Vec<Value> {
    recorded
        .lock()
        .expect("recorder")
        .iter()
        .filter(|(name, _)| name == method)
        .map(|(_, params)| params.clone())
        .collect()
}

#[tokio::test]
async fn backend_reads_agents_from_the_app_server() -> Result<()> {
    let (session, recorded) = fake_app_server().await?;
    let backend = AppServerAgentBackend::new("desktop".to_string(), session.request_handle());

    let agents = backend.list_agents().await?;
    assert_eq!(
        agents
            .iter()
            .map(|agent| (agent.name.as_str(), agent.status))
            .collect::<Vec<_>>(),
        vec![("build", AgentStatus::Running), ("docs", AgentStatus::Idle)]
    );
    assert_eq!(
        agents[0].latest,
        Some(ActivityItem::Command {
            command: "cargo test".to_string(),
            status: ItemStatus::InProgress,
            exit_code: None,
            output: Some("running 3 tests".to_string()),
        })
    );
    // Only the active thread's newest turn is read, with its items.
    let turns: Vec<ThreadTurnsListParams> = params(&recorded, "thread/turns/list")
        .into_iter()
        .map(|params| serde_json::from_value(params).expect("thread/turns/list params"))
        .collect();
    assert_eq!(
        turns,
        vec![crate::app::voice_command_backend::newest_turn_params(
            "t-build", /*with_items*/ true
        )]
    );
    assert_eq!(
        methods(&recorded),
        vec![
            "thread/loaded/list",
            "thread/read",
            "thread/read",
            "thread/turns/list",
        ]
    );

    let report = backend.agent_activity("t-build").await?;
    assert_eq!(report.agent.name, "build");
    assert_eq!(report.recent.len(), 1);
    assert_eq!(
        backend.agent_activity("t-gone").await,
        Err(codex_orchestrator::BackendError::AgentNotFound(
            "t-gone".to_string()
        ))
    );
    Ok(())
}

#[tokio::test]
async fn backend_steers_a_running_turn_and_starts_an_idle_one() -> Result<()> {
    let (session, recorded) = fake_app_server().await?;
    let backend = AppServerAgentBackend::new("desktop".to_string(), session.request_handle());

    assert_eq!(
        backend.steer("t-build", "run the tests").await?,
        SteerOutcome::Steered {
            turn_id: "turn-build".to_string()
        }
    );
    let steer = params(&recorded, "turn/steer");
    assert_eq!(steer.len(), 1);
    assert_eq!(steer[0]["threadId"], "t-build");
    assert_eq!(steer[0]["expectedTurnId"], "turn-build");
    assert_eq!(steer[0]["input"][0]["text"], "run the tests");

    assert_eq!(
        backend.steer("t-docs", "update the readme").await?,
        SteerOutcome::Started {
            turn_id: "turn-new".to_string()
        }
    );
    let start = params(&recorded, "turn/start");
    assert_eq!(start.len(), 1);
    assert_eq!(start[0]["threadId"], "t-docs");
    assert_eq!(start[0]["input"][0]["text"], "update the readme");
    Ok(())
}

#[tokio::test]
async fn backend_interrupts_only_a_running_turn() -> Result<()> {
    let (session, recorded) = fake_app_server().await?;
    let backend = AppServerAgentBackend::new("desktop".to_string(), session.request_handle());

    assert_eq!(
        backend.interrupt("t-docs").await?,
        InterruptOutcome::AlreadyIdle
    );
    assert_eq!(params(&recorded, "turn/interrupt"), Vec::<Value>::new());

    assert_eq!(
        backend.interrupt("t-build").await?,
        InterruptOutcome::Interrupted {
            turn_id: "turn-build".to_string()
        }
    );
    assert_eq!(
        params(&recorded, "turn/interrupt"),
        vec![json!({"threadId": "t-build", "turnId": "turn-build"})]
    );
    Ok(())
}

fn speech(ops: &mut tokio::sync::mpsc::UnboundedReceiver<Op>) -> Vec<String> {
    std::iter::from_fn(|| ops.try_recv().ok())
        .filter_map(|op| match op {
            AppCommand::RealtimeConversationSpeech { text, .. } => Some(text.as_str().to_string()),
            _ => None,
        })
        .collect()
}

/// Send one transcript as the voice widget would, then deliver the
/// orchestrator's answer. Returns the other app events seen meanwhile.
async fn run_spoken(
    app: &mut App,
    tui: &mut crate::tui::Tui,
    app_server: &mut AppServerSession,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    thread_id: ThreadId,
    seq: u64,
    transcript: &str,
) -> Result<Vec<AppEvent>> {
    boxed_future!(app.handle_event(
        tui,
        app_server,
        AppEvent::VoiceCommand {
            thread_id,
            seq,
            transcript: transcript.to_string(),
            approval_prompt_open: false,
        },
    ))
    .await?;
    let mut others = Vec::new();
    loop {
        let event = time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await?
            .expect("app events stay open");
        if matches!(event, AppEvent::VoiceCommandResult { .. }) {
            boxed_future!(app.handle_event(tui, app_server, event)).await?;
            break;
        }
        others.push(event);
    }
    others.extend(std::iter::from_fn(|| events.try_recv().ok()));
    Ok(others)
}

#[tokio::test]
async fn the_app_runs_spoken_commands_through_its_orchestrator() -> Result<()> {
    let (mut app, mut events, mut ops) = make_test_app_with_channels().await;
    app.voice_commands.machine_discovery = MachineDiscovery::Fixed(vec!["studio".to_string()]);
    let mut opus = crate::test_support::TEST_MODEL_PRESETS
        .first()
        .cloned()
        .expect("a preset");
    opus.model = "claude-opus-5-5".to_string();
    opus.display_name = "Claude Opus".to_string();
    app.model_catalog = Arc::new(crate::model_catalog::ModelCatalog::new(vec![opus]));
    let thread_id = ThreadId::new();
    crate::chatwidget::activate_voice_for_thread(&mut app.chat_widget, thread_id);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let (mut app_server, recorded) = fake_app_server().await?;
    let mut seq = 0;
    macro_rules! say {
        ($transcript:expr) => {{
            seq += 1;
            run_spoken(
                &mut app,
                &mut tui,
                &mut app_server,
                &mut events,
                thread_id,
                seq,
                $transcript,
            )
            .await?
        }};
    }

    // A steer waits for "confirm" and sends nothing until then.
    say!("tell build to run the tests");
    let spoken = speech(&mut ops);
    assert_eq!(spoken.len(), 1, "{spoken:?}");
    assert!(spoken[0].starts_with("Tell build on "), "{spoken:?}");
    assert!(
        spoken[0].ends_with(": run the tests. Say confirm."),
        "{spoken:?}"
    );
    assert!(!local_machine_name().is_empty());
    assert_eq!(params(&recorded, "turn/steer"), Vec::<Value>::new());

    say!("confirm");
    assert_eq!(params(&recorded, "turn/steer").len(), 1);
    let spoken = speech(&mut ops);
    assert_eq!(spoken.len(), 1, "{spoken:?}");
    assert!(spoken[0].starts_with("Sent to build on"), "{spoken:?}");

    // The model switch goes through the /model picker's apply.
    let others = say!("switch to opus");
    assert_eq!(
        speech(&mut ops),
        vec!["Switching to Claude Opus.".to_string()]
    );
    assert!(
        others.iter().any(|event| matches!(
            event,
            AppEvent::ApplyModelSelection { model, effort: None } if model == "claude-opus-5-5"
        )),
        "{others:?}"
    );

    // Attaching is not done at runtime; the reply says how.
    say!("connect to studio");
    assert_eq!(
        speech(&mut ops),
        vec!["Run codex remote attach studio.".to_string()]
    );

    say!("list my machines");
    let spoken = speech(&mut ops);
    assert_eq!(spoken.len(), 1, "{spoken:?}");
    assert!(spoken[0].to_lowercase().contains("studio"), "{spoken:?}");
    Ok(())
}
