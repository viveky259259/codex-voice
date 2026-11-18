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

/// One remote machine served over a fake `ssh`: the "remote" `codex app-server
/// proxy` relays to a fake app server on a Unix socket, which answers with
/// `fake_reply` and closes the connection on `thread/read` of `t-drop`.
#[cfg(unix)]
struct FakeSshMachine {
    _dir: tempfile::TempDir,
    target: codex_app_server_client::SshTarget,
    log: std::path::PathBuf,
    pids: std::path::PathBuf,
    recorded: Recorded,
}

#[cfg(unix)]
impl FakeSshMachine {
    async fn start() -> Result<Self> {
        use crate::app::voice_command_remote::tests::write_fake_ssh;
        use crate::app::voice_command_remote::tests::write_remote_codex;
        let dir = tempfile::TempDir::new()?;
        let socket_path = dir.path().join("control.sock");
        let log = dir.path().join("ssh.log");
        let pids = dir.path().join("ssh.pids");
        let fake_ssh = write_fake_ssh(dir.path(), &log, &pids)?;
        let remote_codex = write_remote_codex(dir.path(), &socket_path)?;
        let mut listener = codex_uds::UnixListener::bind(&socket_path).await?;
        let recorded = Recorded::default();
        let sink = Arc::clone(&recorded);
        tokio::spawn(async move {
            while let Ok(stream) = listener.accept().await {
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    let socket = tokio_tungstenite::accept_async(stream).await?;
                    serve_reconnect_requests(socket, move |request| {
                        sink.lock().expect("recorder").push((
                            request.method.clone(),
                            request.params.clone().unwrap_or(Value::Null),
                        ));
                        let drop_connection = request.method == "thread/read"
                            && request
                                .params
                                .as_ref()
                                .is_some_and(|params| params["threadId"] == "t-drop");
                        std::future::ready((!drop_connection).then(|| fake_reply(&request)))
                    })
                    .await
                });
            }
        });
        let mut target = crate::app::voice_command_remote::ssh_target(
            "me@devbox:2222",
            Some(&remote_codex.display().to_string()),
        )
        .map_err(|error| color_eyre::eyre::eyre!(error))?;
        target.ssh_program = Some(fake_ssh);
        Ok(Self {
            _dir: dir,
            target,
            log,
            pids,
            recorded,
        })
    }

    fn ssh_invocations(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn ssh_pids(&self) -> Vec<u32> {
        std::fs::read_to_string(&self.pids)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn remote_backend_drives_a_machine_over_ssh() -> Result<()> {
    use crate::app::voice_command_remote::RemoteLink;
    use codex_orchestrator::MachineLink;
    if !crate::app::voice_command_remote::tests::python3_available() {
        eprintln!("skipping: python3 is required for the stdio relay stub");
        return Ok(());
    }
    let machine = FakeSshMachine::start().await?;
    let link = RemoteLink::new(machine.target.clone())
        .with_connect_wait(std::time::Duration::from_secs(20));
    link.start_connecting();
    let backend = AppServerAgentBackend::new("devbox".to_string(), link);

    // list: the first call waits for the background connect, then reads.
    let agents = backend.list_agents().await?;
    assert_eq!(
        agents
            .iter()
            .map(|agent| (agent.name.as_str(), agent.status))
            .collect::<Vec<_>>(),
        vec![("build", AgentStatus::Running), ("docs", AgentStatus::Idle)]
    );
    assert_eq!(backend.link(), MachineLink::Connected);

    // steer and interrupt reach the remote app server.
    assert_eq!(
        backend.steer("t-build", "run the tests").await?,
        SteerOutcome::Steered {
            turn_id: "turn-build".to_string()
        }
    );
    assert_eq!(
        backend.interrupt("t-build").await?,
        InterruptOutcome::Interrupted {
            turn_id: "turn-build".to_string()
        }
    );
    let steer = params(&machine.recorded, "turn/steer");
    assert_eq!(steer.len(), 1);
    assert_eq!(steer[0]["threadId"], "t-build");
    assert_eq!(steer[0]["expectedTurnId"], "turn-build");
    assert_eq!(steer[0]["input"][0]["text"], "run the tests");
    assert_eq!(
        params(&machine.recorded, "turn/interrupt"),
        vec![json!({"threadId": "t-build", "turnId": "turn-build"})]
    );
    assert_eq!(
        methods(&machine.recorded),
        vec![
            "thread/loaded/list",
            "thread/read",
            "thread/read",
            "thread/turns/list",
            "thread/turns/list",
            "thread/turns/list",
            "turn/interrupt",
            "turn/steer",
        ]
    );

    // One connection served all of that: `daemon start`, then one proxy.
    let invocations = machine.ssh_invocations();
    assert_eq!(invocations.len(), 2, "{invocations:?}");
    let quoted_bin = format!("'{}'", machine.target.remote_codex_bin.as_deref().unwrap());
    assert!(
        invocations[0].ends_with(&format!(
            "-p 2222 -l me -- devbox {quoted_bin} app-server daemon start"
        )),
        "{invocations:?}"
    );
    assert!(
        invocations[1].ends_with(&format!("-- devbox {quoted_bin} app-server proxy")),
        "{invocations:?}"
    );
    assert!(
        invocations
            .iter()
            .all(|invocation| invocation.starts_with("-o BatchMode=yes")),
        "{invocations:?}"
    );

    // The server drops the connection: that call fails for this machine
    // only, and the next one reconnects over a new ssh.
    assert!(matches!(
        backend.agent_activity("t-drop").await,
        Err(codex_orchestrator::BackendError::Unreachable(_))
    ));
    assert_eq!(backend.link(), MachineLink::Unreachable);
    let report = backend.agent_activity("t-docs").await?;
    assert_eq!(report.agent.name, "docs");
    assert_eq!(backend.link(), MachineLink::Connected);
    assert_eq!(machine.ssh_invocations().len(), 4);

    // Dropping the backend kills the proxy's ssh child.
    let proxy_pid = *machine.ssh_pids().last().expect("ssh pid");
    drop(backend);
    #[cfg(target_os = "linux")]
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !crate::app::voice_command_remote::tests::process_ended(proxy_pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "ssh child {proxy_pid} is still running"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = proxy_pid;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn voice_commands_span_this_machine_and_remote_ones() -> Result<()> {
    use crate::app::voice_command_remote::VoiceMachineTarget;
    use crate::app::voice_command_remote::ssh_target;
    use crate::app::voice_command_remote::tests::write_executable;
    use crate::app::voice_commands::orchestrator_for;
    use codex_orchestrator::OutcomeStatus;
    if !crate::app::voice_command_remote::tests::python3_available() {
        eprintln!("skipping: python3 is required for the stdio relay stub");
        return Ok(());
    }
    // This machine: the in-process fake app server (build running, docs idle).
    let (session, local) = fake_app_server().await?;
    let local_backend = AppServerAgentBackend::new("desktop".to_string(), session.request_handle());
    // devbox: the same fake behind a fake ssh. nas: ssh fails.
    let devbox = FakeSshMachine::start().await?;
    let dead_dir = tempfile::TempDir::new()?;
    let dead_ssh = dead_dir.path().join("ssh");
    write_executable(
        &dead_ssh,
        "#!/bin/sh\necho 'ssh: connect to host nas port 22: No route to host' >&2\nexit 255\n",
    )?;
    let mut nas = ssh_target("nas", None).map_err(|error| color_eyre::eyre::eyre!(error))?;
    nas.ssh_program = Some(dead_ssh);
    let mut engine = orchestrator_for(
        Box::new(local_backend),
        vec![
            VoiceMachineTarget {
                name: "devbox".to_string(),
                target: devbox.target.clone(),
            },
            VoiceMachineTarget {
                name: "nas".to_string(),
                target: nas,
            },
        ],
    )
    // Generous for slow CI: the fake ssh starts python for every connection.
    .with_timeout(std::time::Duration::from_secs(10))
    .with_known_machines(vec!["laptop".to_string()]);
    // Let devbox finish connecting (the link waits 1.5 s per call).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let links = engine.machine_links();
        if links[1].1 == Some(codex_orchestrator::MachineLink::Connected) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{links:?}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let listed = engine.handle_utterance("what's running", false).await;
    assert_eq!(listed.status, OutcomeStatus::Done);
    assert!(
        listed.spoken.starts_with("Four agents."),
        "{}",
        listed.spoken
    );
    assert!(
        listed.spoken.ends_with("Nas didn't answer."),
        "{}",
        listed.spoken
    );
    let screen: Vec<&str> = listed.screen.lines().collect();
    assert_eq!(screen.len(), 5, "{}", listed.screen);
    assert!(screen[0].starts_with("build on desktop: running cargo test"));
    assert!(screen[1].starts_with("docs on desktop: idle"));
    assert!(screen[2].starts_with("build on devbox: running cargo test"));
    assert!(screen[3].starts_with("docs on devbox: idle"));
    assert!(
        screen[4].starts_with("nas: unreachable (unreachable: ssh could not connect to `nas`"),
        "{}",
        listed.screen
    );

    // Names are shared across machines, so the engine asks which one.
    let question = engine
        .handle_utterance("tell build to run the tests", false)
        .await;
    assert_eq!(
        question.spoken,
        "Which agent: build on desktop or build on devbox?"
    );
    let asked = engine.handle_utterance("the one on devbox", false).await;
    assert_eq!(asked.status, OutcomeStatus::AwaitingConfirmation);
    assert_eq!(
        asked.spoken,
        "Tell build on devbox: run the tests. Say confirm."
    );
    assert_eq!(params(&devbox.recorded, "turn/steer"), Vec::<Value>::new());
    let sent = engine.handle_utterance("confirm", false).await;
    assert_eq!(sent.spoken, "Sent to build on devbox.");
    assert_eq!(params(&devbox.recorded, "turn/steer").len(), 1);
    assert_eq!(params(&local, "turn/steer"), Vec::<Value>::new());

    let stop = engine.handle_utterance("stop it", false).await;
    assert_eq!(stop.spoken, "Stop build on devbox? Say confirm.");
    let stopped = engine.handle_utterance("confirm", false).await;
    assert_eq!(stopped.spoken, "Stopped build on devbox.");
    assert_eq!(
        params(&devbox.recorded, "turn/interrupt"),
        vec![json!({"threadId": "t-build", "turnId": "turn-build"})]
    );
    assert_eq!(params(&local, "turn/interrupt"), Vec::<Value>::new());

    let machines = engine.handle_utterance("list my machines", false).await;
    assert_eq!(
        machines.spoken,
        "Four machines. Connected: desktop, devbox. Unreachable: nas. Not connected: laptop."
    );
    Ok(())
}
