//! Owns the `claude` process for the current session and answers app-server
//! requests. One session at a time (v1): starting or resuming another thread
//! replaces the process, but never while the current session has a turn
//! running (that request is refused instead).

use crate::BACKEND_NAME;
use crate::ClaudeCodeOptions;
use crate::MODEL_PROVIDER;
use crate::approvals;
use crate::approvals::PendingApproval;
use crate::mapping;
use crate::mapping::TurnMapper;
use crate::models;
use crate::state::SavedSettings;
use crate::state::SessionStore;
use crate::wire;
use crate::wire::Launch;
use codex_app_server_client::AgentBackendCommand;
use codex_app_server_client::AgentBackendOperation;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::RequestResult;
use codex_app_server_protocol as protocol;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_protocol::openai_models::ReasoningEffort;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Instant;

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const SERVER_ERROR: i64 = -32000;
const STDERR_TAIL_LINES: usize = 20;

type Responder = oneshot::Sender<io::Result<RequestResult>>;

enum ProcessEvent {
    Line(String),
    Closed,
}

struct ActiveTurn {
    mapper: TurnMapper,
    started_at: i64,
    interrupt_requested: bool,
}

struct Session {
    thread_id: String,
    /// A thread/start or thread/resume owns this process.
    adopted: bool,
    child: Child,
    stdin: ChildStdin,
    events: mpsc::UnboundedReceiver<ProcessEvent>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    init_request_id: Option<String>,
    next_control: u64,
    turn: Option<ActiveTurn>,
    approvals: HashMap<RequestId, PendingApproval>,
    usage_total: protocol::TokenUsageBreakdown,
}

#[derive(Clone)]
struct ThreadInfo {
    id: String,
    preview: String,
    name: Option<String>,
    created_at: i64,
    updated_at: i64,
    /// The CLI saved at least one message, so `--resume` can find the session.
    saved: bool,
}

struct Worker {
    options: ClaudeCodeOptions,
    output: mpsc::UnboundedSender<AppServerEvent>,
    threads: Vec<ThreadInfo>,
    session: Option<Session>,
    /// `models` from the CLI's initialize response, once known.
    reported_models: Option<Vec<Value>>,
    /// Set once an initialize attempt has finished, successfully or not.
    init_settled: bool,
    claude_code_version: Option<String>,
    /// The model the CLI uses: `None` is Claude Code's default.
    current_model: Option<String>,
    /// The effort the CLI uses: `None` is the model's default.
    current_effort: Option<ReasoningEffort>,
    /// Each session's model and effort, for a resume.
    store: SessionStore,
    waiting_model_lists: Vec<Responder>,
    init_deadline: Option<Instant>,
    next_approval: u64,
}

enum Step {
    Command(Option<AgentBackendCommand>),
    Process(ProcessEvent),
    InitTimeout,
    OutputClosed,
}

pub(crate) async fn run(
    options: ClaudeCodeOptions,
    mut commands: mpsc::Receiver<AgentBackendCommand>,
    output: mpsc::UnboundedSender<AppServerEvent>,
) {
    let mut worker = Worker {
        current_model: options.model.clone(),
        current_effort: None,
        store: SessionStore::new(options.state_dir.as_deref()),
        options,
        output,
        threads: Vec::new(),
        session: None,
        reported_models: None,
        init_settled: false,
        claude_code_version: None,
        waiting_model_lists: Vec::new(),
        init_deadline: None,
        next_approval: 0,
    };
    loop {
        let deadline = worker.init_deadline;
        let step = tokio::select! {
            command = commands.recv() => Step::Command(command),
            event = next_process_event(&mut worker.session) => Step::Process(event),
            () = sleep_until(deadline) => Step::InitTimeout,
            () = worker.output.closed() => Step::OutputClosed,
        };
        match step {
            Step::Command(None) | Step::OutputClosed => break,
            Step::Command(Some(command)) => {
                if !worker.handle_command(command).await {
                    break;
                }
            }
            Step::Process(ProcessEvent::Line(line)) => worker.on_line(&line).await,
            Step::Process(ProcessEvent::Closed) => worker.on_process_closed().await,
            Step::InitTimeout => {
                worker.init_deadline = None;
                worker.init_settled = true;
                worker.answer_model_lists();
            }
        }
    }
    worker.stop_session().await;
}

async fn next_process_event(session: &mut Option<Session>) -> ProcessEvent {
    match session {
        Some(session) => session.events.recv().await.unwrap_or(ProcessEvent::Closed),
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn now_s() -> i64 {
    now_ms() / 1000
}

fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn error(code: i64, message: impl Into<String>) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code,
        message: message.into(),
        data: None,
    }
}

fn ok(value: impl Serialize) -> RequestResult {
    serde_json::to_value(value).map_err(|err| error(SERVER_ERROR, err.to_string()))
}

impl Worker {
    fn notify(&self, notification: ServerNotification) {
        let _ = self
            .output
            .send(AppServerEvent::ServerNotification(Box::new(notification)));
    }

    fn notify_all(&self, notifications: Vec<ServerNotification>) {
        for notification in notifications {
            self.notify(notification);
        }
    }

    fn warn(&self, thread_id: Option<&str>, message: String) {
        self.notify(ServerNotification::Warning(protocol::WarningNotification {
            thread_id: thread_id.map(str::to_string),
            message,
        }));
    }

    /// Returns false to stop the worker.
    async fn handle_command(&mut self, command: AgentBackendCommand) -> bool {
        let AgentBackendCommand {
            operation,
            response,
        } = command;
        let result = match operation {
            AgentBackendOperation::Shutdown => {
                self.stop_session().await;
                let _ = response.send(Ok(Ok(Value::Null)));
                return false;
            }
            AgentBackendOperation::Notify(_) => Ok(Value::Null),
            AgentBackendOperation::Resolve { request_id, result } => {
                self.answer_approval(&request_id, Some(&result), None).await
            }
            AgentBackendOperation::Reject { request_id, error } => {
                self.answer_approval(&request_id, None, Some(&error.message))
                    .await
            }
            AgentBackendOperation::Request(request) => match *request {
                ClientRequest::ModelList { .. } => {
                    self.model_list(response).await;
                    return true;
                }
                request => self.handle_request(request).await,
            },
        };
        let _ = response.send(Ok(result));
        true
    }

    async fn handle_request(&mut self, request: ClientRequest) -> RequestResult {
        match request {
            ClientRequest::GetAccount { .. } => ok(protocol::GetAccountResponse {
                account: None,
                user_id: None,
                requires_openai_auth: false,
                workspace_routing: None,
            }),
            ClientRequest::ConfigRequirementsRead { .. } => {
                Ok(serde_json::json!({ "requirements": null }))
            }
            // The TUI reads config layers at startup to check folder trust.
            // Claude Code has no Codex config layers, so report none.
            ClientRequest::ConfigRead { .. } => Ok(serde_json::json!({
                "config": {},
                "origins": {},
                "layers": [],
            })),
            // The TUI loads these at startup. Claude Code has its own skills,
            // hooks and plugins, none of them Codex's: report empty lists.
            ClientRequest::SkillsList { params, .. } => ok(protocol::SkillsListResponse {
                data: self
                    .cwds(params.cwds)
                    .into_iter()
                    .map(|cwd| protocol::SkillsListEntry {
                        cwd,
                        skills: Vec::new(),
                        errors: Vec::new(),
                    })
                    .collect(),
            }),
            ClientRequest::HooksList { params, .. } => ok(protocol::HooksListResponse {
                data: self
                    .cwds(params.cwds)
                    .into_iter()
                    .map(|cwd| protocol::HooksListEntry {
                        cwd,
                        hooks: Vec::new(),
                        warnings: Vec::new(),
                        errors: Vec::new(),
                    })
                    .collect(),
            }),
            ClientRequest::PluginInstalled { .. } => ok(protocol::PluginInstalledResponse {
                marketplaces: Vec::new(),
                marketplace_load_errors: Vec::new(),
            }),
            ClientRequest::ThreadStart { .. } => self.thread_start().await,
            ClientRequest::ThreadResume { params, .. } => self.thread_resume(params).await,
            ClientRequest::ThreadRead { params, .. } => ok(protocol::ThreadReadResponse {
                thread: self.thread(&params.thread_id),
                read_state: None,
            }),
            ClientRequest::ThreadList { .. } => {
                let mut threads: Vec<ThreadInfo> = self.threads.clone();
                threads.sort_by_key(|thread| std::cmp::Reverse(thread.updated_at));
                ok(protocol::ThreadListResponse {
                    data: threads
                        .iter()
                        .map(|thread| self.thread(&thread.id))
                        .collect(),
                    read_states: None,
                    next_cursor: None,
                    backwards_cursor: None,
                })
            }
            ClientRequest::ThreadLoadedList { .. } => ok(protocol::ThreadLoadedListResponse {
                data: self
                    .session
                    .iter()
                    .filter(|session| session.adopted)
                    .map(|session| session.thread_id.clone())
                    .collect(),
                next_cursor: None,
            }),
            ClientRequest::ThreadUnsubscribe { params, .. } => {
                let loaded = self.session.as_ref().is_some_and(|session| {
                    session.adopted && session.thread_id == params.thread_id
                });
                if loaded {
                    self.stop_session().await;
                }
                ok(protocol::ThreadUnsubscribeResponse {
                    status: if loaded {
                        protocol::ThreadUnsubscribeStatus::Unsubscribed
                    } else {
                        protocol::ThreadUnsubscribeStatus::NotSubscribed
                    },
                })
            }
            ClientRequest::ThreadSetName { params, .. } => {
                if let Some(thread) = self.threads.iter_mut().find(|t| t.id == params.thread_id) {
                    thread.name = Some(params.name.trim().to_string());
                }
                self.notify(ServerNotification::ThreadNameUpdated(
                    protocol::ThreadNameUpdatedNotification {
                        thread_id: params.thread_id,
                        thread_name: Some(params.name.trim().to_string()),
                    },
                ));
                ok(protocol::ThreadSetNameResponse {})
            }
            ClientRequest::ThreadSettingsUpdate { params, .. } => {
                if let Some(model) = params.model.as_deref() {
                    self.switch_model(model).await?;
                }
                if let Some(effort) = params.effort.as_ref() {
                    self.switch_effort(effort).await?;
                }
                self.save_settings();
                ok(protocol::ThreadSettingsUpdateResponse {})
            }
            ClientRequest::TurnStart { params, .. } => self.turn_start(params).await,
            ClientRequest::TurnInterrupt { params, .. } => self.turn_interrupt(params).await,
            other => Err(error(
                METHOD_NOT_FOUND,
                format!(
                    "{} is not supported in {BACKEND_NAME} sessions.",
                    other.method_name()
                ),
            )),
        }
    }

    // ---- models ----------------------------------------------------------

    fn merged_models(&self) -> Vec<models::ClaudeModel> {
        models::merge_models(self.reported_models.as_deref())
    }

    async fn model_list(&mut self, response: Responder) {
        if self.reported_models.is_none() && !self.init_settled {
            if self.session.is_none()
                && let Err(err) = self.spawn_new().await
            {
                tracing::warn!(%err, "could not start Claude Code to list models");
                self.init_settled = true;
            }
            if self.session.is_some() && !self.init_settled {
                self.waiting_model_lists.push(response);
                self.init_deadline
                    .get_or_insert_with(|| Instant::now() + self.options.init_timeout);
                return;
            }
        }
        let _ = response.send(Ok(self.model_list_page()));
    }

    fn model_list_page(&self) -> RequestResult {
        let current = self
            .current_model
            .as_deref()
            .unwrap_or(models::DEFAULT_MODEL);
        ok(protocol::ModelListResponse {
            data: models::protocol_models(&self.merged_models(), current),
            next_cursor: None,
        })
    }

    fn answer_model_lists(&mut self) {
        let page = self.model_list_page();
        for response in self.waiting_model_lists.drain(..) {
            let _ = response.send(Ok(page.clone()));
        }
    }

    /// `/model`, a spoken "switch to opus", or a turn's model: `set_model` on the
    /// running CLI, else the `--model` of the next process.
    async fn switch_model(&mut self, requested: &str) -> Result<(), JSONRPCErrorError> {
        let model = self.model_for(requested)?;
        if model == self.current_model {
            return Ok(());
        }
        if let Some(session) = self.session.as_mut() {
            let id = control_id(session);
            write_line(
                &mut session.stdin,
                &wire::control_request(&id, wire::set_model_request(model.as_deref())),
            )
            .await
            .map_err(|err| {
                error(
                    SERVER_ERROR,
                    format!("Could not switch the {BACKEND_NAME} model: {err}"),
                )
            })?;
        }
        self.current_model = model;
        Ok(())
    }

    /// The `--model` / `set_model` value for a requested model or family name.
    fn model_for(&self, requested: &str) -> Result<Option<String>, JSONRPCErrorError> {
        let models = self.merged_models();
        if requested == models::DEFAULT_MODEL {
            Ok(None)
        } else if let Some(model) = models::resolve(&models, requested) {
            Ok(Some(model.id.clone()))
        } else if models::is_claude_model_id(requested) {
            Ok(Some(requested.to_string()))
        } else {
            Err(error(
                INVALID_PARAMS,
                format!("{requested} is not a Claude model."),
            ))
        }
    }

    /// The effort picker or a turn's effort: `apply_flag_settings` with
    /// `effortLevel` on the running CLI (the SDK's `applyFlagSettings`), else
    /// the `--effort` of the next process.
    async fn switch_effort(&mut self, effort: &ReasoningEffort) -> Result<(), JSONRPCErrorError> {
        let Some(level) = models::claude_effort(effort) else {
            // Not a level the picker offers for Claude models: nothing to apply.
            tracing::debug!(%effort, "ignoring an effort Claude Code has no level for");
            return Ok(());
        };
        if self.current_effort.as_ref() == Some(effort) {
            return Ok(());
        }
        if let Some(session) = self.session.as_mut() {
            let id = control_id(session);
            write_line(
                &mut session.stdin,
                &wire::control_request(&id, wire::set_effort_request(Some(level))),
            )
            .await
            .map_err(|err| {
                error(
                    SERVER_ERROR,
                    format!("Could not change the {BACKEND_NAME} effort: {err}"),
                )
            })?;
        }
        self.current_effort = Some(effort.clone());
        Ok(())
    }

    fn effort_level(&self) -> Option<&'static str> {
        self.current_effort.as_ref().and_then(models::claude_effort)
    }

    /// Remembers the adopted session's model and effort for a later resume.
    fn save_settings(&self) {
        let Some(session) = self.session.as_ref().filter(|session| session.adopted) else {
            return;
        };
        self.store.put(
            &session.thread_id,
            SavedSettings {
                model: self.current_model.clone(),
                effort: self.effort_level().map(str::to_string),
                updated_at: now_s(),
            },
        );
    }

    /// Restores a session's saved model and effort before it is resumed.
    fn restore_settings(&mut self, thread_id: &str) {
        let Some(saved) = self.store.get(thread_id) else {
            return;
        };
        self.current_model = saved.model;
        self.current_effort = saved
            .effort
            .as_deref()
            .and_then(|effort| ReasoningEffort::from_str(effort).ok());
    }

    /// A request that would replace the process while a turn runs is refused:
    /// stopping the `claude` process would stop that turn.
    fn refuse_if_busy(&self, thread_id: Option<&str>) -> Result<(), JSONRPCErrorError> {
        let busy = self.session.as_ref().is_some_and(|session| {
            session.adopted
                && session.turn.is_some()
                && thread_id.is_none_or(|thread_id| thread_id != session.thread_id)
        });
        if busy {
            return Err(error(
                INVALID_PARAMS,
                format!(
                    "{BACKEND_NAME} is still working in this session. Wait for it or stop it (Esc), then try again."
                ),
            ));
        }
        Ok(())
    }

    // ---- threads ---------------------------------------------------------

    fn thread(&self, thread_id: &str) -> protocol::Thread {
        let info = self.threads.iter().find(|thread| thread.id == thread_id);
        let loaded = self
            .session
            .as_ref()
            .filter(|session| session.adopted && session.thread_id == thread_id);
        let status = match loaded {
            Some(session) if session.turn.is_some() => protocol::ThreadStatus::Active {
                active_flags: Vec::new(),
            },
            Some(_) => protocol::ThreadStatus::Idle,
            None => protocol::ThreadStatus::NotLoaded,
        };
        let now = now_s();
        protocol::Thread {
            id: thread_id.to_string(),
            environments: None,
            extra: None,
            session_id: thread_id.to_string(),
            forked_from_id: None,
            attached_to_thread_id: None,
            parent_thread_id: None,
            preview: info.map(|info| info.preview.clone()).unwrap_or_default(),
            ephemeral: false,
            section: None,
            section_entered_at: None,
            project_id: None,
            history_mode: protocol::ThreadHistoryMode::Legacy,
            model_provider: MODEL_PROVIDER.to_string(),
            model: Some(self.model_label()),
            reasoning_effort: self.current_effort.clone(),
            created_at: info.map_or(now, |info| info.created_at),
            updated_at: info.map_or(now, |info| info.updated_at),
            recency_at: None,
            status,
            path: None,
            cwd: self.cwd(),
            cli_version: self.claude_code_version.clone().unwrap_or_default(),
            originator: Some(BACKEND_NAME.to_string()),
            source: protocol::SessionSource::Cli,
            can_accept_direct_input: None,
            thread_source: None,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: info.and_then(|info| info.name.clone()),
            daybreak_enabled: None,
            turns: Vec::new(),
        }
    }

    fn model_label(&self) -> String {
        self.current_model
            .clone()
            .unwrap_or_else(|| models::DEFAULT_MODEL.to_string())
    }

    /// The requested working directories, or the session's.
    fn cwds(&self, requested: Vec<PathBuf>) -> Vec<PathBuf> {
        if requested.is_empty() {
            vec![self.options.cwd.clone()]
        } else {
            requested
        }
    }

    fn cwd(&self) -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path(&self.options.cwd)
            .or_else(|_| AbsolutePathBuf::current_dir())
            .unwrap_or_else(|_| AbsolutePathBuf::from_absolute_path("/").expect("root is absolute"))
    }

    fn remember_thread(&mut self, thread_id: &str) {
        if !self.threads.iter().any(|thread| thread.id == thread_id) {
            let now = now_s();
            self.threads.push(ThreadInfo {
                id: thread_id.to_string(),
                preview: String::new(),
                name: None,
                created_at: now,
                updated_at: now,
                saved: false,
            });
        }
    }

    async fn thread_start(&mut self) -> RequestResult {
        // A second thread/start (a hidden title or helper thread, `/new`)
        // would replace the process and stop the running turn. Refuse it.
        self.refuse_if_busy(/*thread_id*/ None)?;
        let reuse = self
            .session
            .as_ref()
            .is_some_and(|session| !session.adopted);
        if !reuse {
            self.spawn_new()
                .await
                .map_err(|err| self.spawn_error(err))?;
        }
        let Some(session) = self.session.as_mut() else {
            return Err(error(
                SERVER_ERROR,
                format!("{BACKEND_NAME} did not start."),
            ));
        };
        session.adopted = true;
        let thread_id = session.thread_id.clone();
        self.remember_thread(&thread_id);
        let thread = self.thread(&thread_id);
        let response = protocol::ThreadStartResponse {
            thread: thread.clone(),
            model: self.model_label(),
            model_provider: MODEL_PROVIDER.to_string(),
            service_tier: None,
            disabled_plugin_ids: Vec::new(),
            cwd: self.cwd(),
            runtime_workspace_roots: Vec::new(),
            instruction_sources: Vec::new(),
            approval_policy: protocol::AskForApproval::OnRequest,
            approvals_reviewer: protocol::ApprovalsReviewer::User,
            sandbox: external_sandbox(),
            active_permission_profile: None,
            reasoning_effort: self.current_effort.clone(),
            multi_agent_mode: Default::default(),
        };
        self.notify(ServerNotification::ThreadStarted(
            protocol::ThreadStartedNotification { thread },
        ));
        ok(response)
    }

    async fn thread_resume(&mut self, params: protocol::ThreadResumeParams) -> RequestResult {
        let thread_id = params.thread_id;
        if uuid::Uuid::parse_str(&thread_id).is_err() {
            return Err(error(
                INVALID_PARAMS,
                format!("{thread_id} is not a {BACKEND_NAME} session id."),
            ));
        }
        let loaded = self
            .session
            .as_ref()
            .is_some_and(|session| session.thread_id == thread_id);
        if !loaded {
            self.refuse_if_busy(Some(&thread_id))?;
            // The model in use before quitting, unless the resume names one.
            self.restore_settings(&thread_id);
            if let Some(model) = params
                .model
                .as_deref()
                .filter(|model| models::is_claude_model_id(model))
            {
                self.current_model = self.model_for(model)?;
            }
            self.spawn(Launch::Resume {
                session_id: thread_id.clone(),
            })
            .await
            .map_err(|err| self.spawn_error(err))?;
        }
        if let Some(session) = self.session.as_mut() {
            session.adopted = true;
        }
        self.remember_thread(&thread_id);
        if let Some(thread) = self
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        {
            thread.saved = true;
        }
        ok(protocol::ThreadResumeResponse {
            thread: self.thread(&thread_id),
            model: self.model_label(),
            model_provider: MODEL_PROVIDER.to_string(),
            service_tier: None,
            disabled_plugin_ids: Vec::new(),
            cwd: self.cwd(),
            runtime_workspace_roots: Vec::new(),
            instruction_sources: Vec::new(),
            approval_policy: protocol::AskForApproval::OnRequest,
            approvals_reviewer: protocol::ApprovalsReviewer::User,
            sandbox: external_sandbox(),
            active_permission_profile: None,
            reasoning_effort: self.current_effort.clone(),
            collaboration_mode: None,
            multi_agent_mode: Default::default(),
            initial_turns_page: None,
            turns_backwards_cursor: None,
            items_backwards_cursor: None,
        })
    }

    // ---- turns -----------------------------------------------------------

    async fn turn_start(&mut self, params: protocol::TurnStartParams) -> RequestResult {
        let thread_id = params.thread_id.clone();
        let mut text = Vec::new();
        for input in &params.input {
            match input {
                protocol::UserInput::Text { text: part, .. } => text.push(part.as_str()),
                _ => {
                    return Err(error(
                        INVALID_PARAMS,
                        format!("{BACKEND_NAME} sessions accept text messages only for now."),
                    ));
                }
            }
        }
        let text = text.join("\n");
        if text.trim().is_empty() {
            return Err(error(INVALID_PARAMS, "The message is empty."));
        }
        let running = self
            .session
            .as_ref()
            .is_some_and(|session| session.adopted && session.thread_id == thread_id);
        if !running {
            if !self.threads.iter().any(|thread| thread.id == thread_id) {
                return Err(error(
                    INVALID_PARAMS,
                    format!("Start or resume this {BACKEND_NAME} session first."),
                ));
            }
            self.refuse_if_busy(Some(&thread_id))?;
            // The process exited since: continue the same session.
            let launch = if self.session_saved(&thread_id) {
                Launch::Resume {
                    session_id: thread_id.clone(),
                }
            } else {
                Launch::New {
                    session_id: thread_id.clone(),
                }
            };
            self.spawn(launch)
                .await
                .map_err(|err| self.spawn_error(err))?;
            if let Some(session) = self.session.as_mut() {
                session.adopted = true;
            }
        }
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.turn.is_some())
        {
            return Err(error(
                INVALID_PARAMS,
                format!("{BACKEND_NAME} is still working on the previous message."),
            ));
        }
        if let Some(model) = params.model.as_deref()
            && models::is_claude_model_id(model)
        {
            // A model the user picked before the session reported its list.
            if let Err(err) = self.switch_model(model).await {
                tracing::debug!(message = err.message, "ignoring the turn's model");
            }
        }
        // The level shown in the footer is the one the turn carries: apply it.
        if let Some(effort) = params.effort.as_ref()
            && let Err(err) = self.switch_effort(effort).await
        {
            tracing::debug!(message = err.message, "ignoring the turn's effort");
        }
        self.save_settings();
        let Some(session) = self.session.as_mut() else {
            return Err(error(
                SERVER_ERROR,
                format!("{BACKEND_NAME} is not running."),
            ));
        };
        write_line(&mut session.stdin, &wire::user_message(&text))
            .await
            .map_err(|err| {
                error(
                    SERVER_ERROR,
                    format!("Could not send the message to {BACKEND_NAME}: {err}"),
                )
            })?;
        let turn_id = uuid::Uuid::new_v4().to_string();
        let started_at = now_s();
        session.turn = Some(ActiveTurn {
            mapper: TurnMapper::new(&thread_id, &turn_id, &self.options.cwd),
            started_at,
            interrupt_requested: false,
        });
        if let Some(thread) = self
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        {
            if thread.preview.is_empty() {
                thread.preview = text.chars().take(120).collect();
            }
            thread.updated_at = started_at;
            thread.saved = true;
        }
        let turn = protocol::Turn {
            id: turn_id.clone(),
            root_turn_id: None,
            items: Vec::new(),
            items_view: Default::default(),
            status: protocol::TurnStatus::InProgress,
            error: None,
            started_at: Some(started_at),
            completed_at: None,
            duration_ms: None,
        };
        self.notify(ServerNotification::ThreadStatusChanged(
            protocol::ThreadStatusChangedNotification {
                thread_id: thread_id.clone(),
                status: protocol::ThreadStatus::Active {
                    active_flags: Vec::new(),
                },
            },
        ));
        self.notify(ServerNotification::TurnStarted(
            protocol::TurnStartedNotification {
                thread_id: thread_id.clone(),
                turn: turn.clone(),
            },
        ));
        let user_item = protocol::ThreadItemPayload::UserMessage {
            id: uuid::Uuid::new_v4().to_string(),
            client_id: params.client_user_message_id.clone(),
            content: params.input.clone(),
        };
        let now = now_ms();
        self.notify(ServerNotification::ItemStarted(
            protocol::ItemStartedNotification::new(
                thread_id.clone(),
                turn_id.clone(),
                user_item.clone(),
                now,
            ),
        ));
        self.notify(ServerNotification::ItemCompleted(
            protocol::ItemCompletedNotification::new(thread_id, turn_id, user_item, Some(now), now),
        ));
        ok(protocol::TurnStartResponse { turn })
    }

    fn session_saved(&self, thread_id: &str) -> bool {
        self.threads
            .iter()
            .any(|thread| thread.id == thread_id && thread.saved)
    }

    async fn turn_interrupt(&mut self, params: protocol::TurnInterruptParams) -> RequestResult {
        if let Some(session) = self.session.as_mut()
            && session.thread_id == params.thread_id
            && let Some(turn) = session.turn.as_mut()
        {
            turn.interrupt_requested = true;
            let id = control_id(session);
            write_line(
                &mut session.stdin,
                &wire::control_request(&id, wire::interrupt_request()),
            )
            .await
            .map_err(|err| {
                error(
                    SERVER_ERROR,
                    format!("Could not interrupt {BACKEND_NAME}: {err}"),
                )
            })?;
        }
        ok(protocol::TurnInterruptResponse {})
    }

    fn finish_turn(&mut self, result: Option<&Value>, failure: Option<String>) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let Some(mut turn) = session.turn.take() else {
            return;
        };
        let thread_id = session.thread_id.clone();
        let turn_id = turn.mapper.turn_id().to_string();
        let mut notifications = turn.mapper.finish(now_ms());
        let withdrawn: Vec<RequestId> = session
            .approvals
            .iter()
            .filter(|(_, pending)| pending.turn_id == turn_id)
            .map(|(id, _)| id.clone())
            .collect();
        for request_id in withdrawn {
            session.approvals.remove(&request_id);
            notifications.push(ServerNotification::ServerRequestResolved(
                protocol::ServerRequestResolvedNotification {
                    thread_id: thread_id.clone(),
                    request_id,
                },
            ));
        }
        if let Some(usage) = result.and_then(|result| result.get("usage")) {
            let last = mapping::usage_breakdown(usage);
            let total = &mut session.usage_total;
            total.total_tokens += last.total_tokens;
            total.input_tokens += last.input_tokens;
            total.cached_input_tokens += last.cached_input_tokens;
            total.cache_write_input_tokens += last.cache_write_input_tokens;
            total.output_tokens += last.output_tokens;
            notifications.push(ServerNotification::ThreadTokenUsageUpdated(
                protocol::ThreadTokenUsageUpdatedNotification {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    token_usage: protocol::ThreadTokenUsage {
                        total: total.clone(),
                        last,
                        model_context_window: None,
                    },
                },
            ));
        }
        let (status, turn_error) = if turn.interrupt_requested {
            (protocol::TurnStatus::Interrupted, None)
        } else if let Some(err) = result.and_then(mapping::result_error) {
            (protocol::TurnStatus::Failed, Some(err))
        } else if let Some(message) = failure {
            (
                protocol::TurnStatus::Failed,
                Some(protocol::TurnError {
                    message,
                    codex_error_info: None,
                    additional_details: None,
                    misalignment: None,
                }),
            )
        } else {
            (protocol::TurnStatus::Completed, None)
        };
        // As the Codex app server does: a completed turn carries its final
        // agent message (the TUI speaks and records it from here); other
        // turns carry no items.
        let (items, items_view) = match turn.mapper.last_agent_message() {
            Some(item) if status == protocol::TurnStatus::Completed => {
                (vec![item.clone()], protocol::TurnItemsView::Summary)
            }
            _ => (Vec::new(), protocol::TurnItemsView::NotLoaded),
        };
        let completed_at = now_s();
        notifications.push(ServerNotification::TurnCompleted(
            protocol::TurnCompletedNotification {
                thread_id: thread_id.clone(),
                turn: protocol::Turn {
                    id: turn_id,
                    root_turn_id: None,
                    items,
                    items_view,
                    status,
                    error: turn_error,
                    started_at: Some(turn.started_at),
                    completed_at: Some(completed_at),
                    duration_ms: Some((completed_at - turn.started_at) * 1000),
                },
            },
        ));
        notifications.push(ServerNotification::ThreadStatusChanged(
            protocol::ThreadStatusChangedNotification {
                thread_id,
                status: protocol::ThreadStatus::Idle,
            },
        ));
        self.notify_all(notifications);
    }

    // ---- approvals -------------------------------------------------------

    async fn on_permission_request(&mut self, control_request_id: &str, request: &Value) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let Some(turn) = session.turn.as_mut() else {
            let tool_use_id = request
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let answer = wire::control_success(
                control_request_id,
                wire::permission_deny(tool_use_id, "No turn is active.", false),
            );
            let _ = write_line(&mut session.stdin, &answer).await;
            return;
        };
        let now = now_ms();
        let tool_use_id = request
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or(control_request_id)
            .to_string();
        let tool_name = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        let started = turn
            .mapper
            .start_tool(&tool_use_id, &tool_name, input, now, &read_file);
        self.next_approval += 1;
        let request_id = RequestId::String(format!("claude-approval-{}", self.next_approval));
        let thread_id = session.thread_id.clone();
        let turn_id = turn.mapper.turn_id().to_string();
        let (server_request, pending) = approvals::approval_request(
            request_id.clone(),
            control_request_id,
            request,
            &thread_id,
            &turn_id,
            &self.options.cwd,
            now,
        );
        session.approvals.insert(request_id, pending);
        self.notify_all(started);
        self.send_server_request(server_request);
    }

    fn send_server_request(&self, request: ServerRequest) {
        let _ = self
            .output
            .send(AppServerEvent::ServerRequest(Box::new(request)));
    }

    async fn answer_approval(
        &mut self,
        request_id: &RequestId,
        result: Option<&Value>,
        rejection: Option<&str>,
    ) -> RequestResult {
        let Some(session) = self.session.as_mut() else {
            return Err(error(INVALID_PARAMS, "This approval is no longer pending."));
        };
        let Some(pending) = session.approvals.remove(request_id) else {
            return Err(error(INVALID_PARAMS, "This approval is no longer pending."));
        };
        let decision = match (result, rejection) {
            (Some(result), _) => approvals::decision_from_response(&pending, result),
            (None, message) => approvals::Decision::Deny {
                message: format!(
                    "The approval request was rejected: {}",
                    message.unwrap_or("no reason given")
                ),
                interrupt: false,
            },
        };
        if decision.is_deny()
            && let Some(turn) = session.turn.as_mut()
        {
            turn.mapper.mark_declined(&pending.tool_use_id);
        }
        let answer = wire::control_success(
            &pending.control_request_id,
            decision.permission_result(&pending.tool_use_id),
        );
        write_line(&mut session.stdin, &answer)
            .await
            .map_err(|err| {
                error(
                    SERVER_ERROR,
                    format!("Could not answer {BACKEND_NAME}: {err}"),
                )
            })?;
        self.notify(ServerNotification::ServerRequestResolved(
            protocol::ServerRequestResolvedNotification {
                thread_id: pending.thread_id,
                request_id: request_id.clone(),
            },
        ));
        Ok(Value::Null)
    }

    // ---- the process -----------------------------------------------------

    async fn spawn_new(&mut self) -> io::Result<()> {
        self.spawn(Launch::New {
            session_id: uuid::Uuid::new_v4().to_string(),
        })
        .await
    }

    fn spawn_error(&self, err: io::Error) -> JSONRPCErrorError {
        error(
            SERVER_ERROR,
            format!(
                "Could not start {BACKEND_NAME} ({}): {err}. Install Claude Code and run `claude` once to log in.",
                self.options.claude_bin.display()
            ),
        )
    }

    async fn spawn(&mut self, launch: Launch) -> io::Result<()> {
        self.stop_session().await;
        let mut command = Command::new(&self.options.claude_bin);
        command
            .args(wire::claude_args(
                self.current_model.as_deref(),
                self.effort_level(),
                &launch,
            ))
            .current_dir(&self.options.cwd)
            .envs(self.options.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(io::Error::other("the process has no stdio pipes"));
        };
        let (events, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if events.send(ProcessEvent::Line(line)).is_err() {
                    return;
                }
            }
            let _ = events.send(ProcessEvent::Closed);
        });
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let tail = Arc::clone(&stderr_tail);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "codex_claude_agent_backend::stderr", "{line}");
                let mut tail = tail
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if tail.len() == STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });
        let init_request_id = "init-1".to_string();
        write_line(
            &mut stdin,
            &wire::control_request(&init_request_id, wire::initialize_request()),
        )
        .await?;
        self.session = Some(Session {
            thread_id: launch.session_id().to_string(),
            adopted: false,
            child,
            stdin,
            events: receiver,
            stderr_tail,
            init_request_id: Some(init_request_id),
            next_control: 1,
            turn: None,
            approvals: HashMap::new(),
            usage_total: mapping::usage_breakdown(&Value::Null),
        });
        Ok(())
    }

    async fn stop_session(&mut self) {
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.turn.is_some())
        {
            self.finish_turn(None, Some(format!("{BACKEND_NAME} was stopped.")));
        }
        if let Some(mut session) = self.session.take() {
            let _ = session.child.start_kill();
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(2), session.child.wait()).await;
        }
    }

    async fn on_process_closed(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), session.child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        let tail: Vec<String> = session
            .stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect();
        let thread_id = session.thread_id.clone();
        let adopted = session.adopted;
        let mut message = match status {
            Some(status) => format!("{BACKEND_NAME} exited ({status})."),
            None => format!("{BACKEND_NAME} closed its output."),
        };
        if !tail.is_empty() {
            message.push('\n');
            message.push_str(&tail.join("\n"));
        }
        let had_turn = session.turn.is_some();
        if had_turn {
            self.finish_turn(None, Some(message.clone()));
        } else if adopted {
            self.warn(Some(&thread_id), message);
        }
        self.session = None;
        if !self.init_settled {
            self.init_settled = true;
            self.init_deadline = None;
            self.answer_model_lists();
        }
    }

    async fn on_line(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            tracing::debug!("ignoring a line that is not JSON from {BACKEND_NAME}");
            return;
        };
        match message.get("type").and_then(Value::as_str) {
            Some("control_response") => self.on_control_response(&message),
            Some("control_request") => {
                let id = message
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let request = message.get("request").cloned().unwrap_or(Value::Null);
                if request.get("subtype").and_then(Value::as_str) == Some("can_use_tool") {
                    self.on_permission_request(&id, &request).await;
                } else if let Some(session) = self.session.as_mut() {
                    let subtype = request
                        .get("subtype")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let answer = wire::control_error(
                        &id,
                        &format!("{subtype} is not supported by this client"),
                    );
                    let _ = write_line(&mut session.stdin, &answer).await;
                }
            }
            Some("control_cancel_request") => {
                let id = message
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut resolved = Vec::new();
                if let Some(session) = self.session.as_mut() {
                    let withdrawn: Vec<RequestId> = session
                        .approvals
                        .iter()
                        .filter(|(_, pending)| pending.control_request_id == id)
                        .map(|(request_id, _)| request_id.clone())
                        .collect();
                    for request_id in withdrawn {
                        session.approvals.remove(&request_id);
                        resolved.push(ServerNotification::ServerRequestResolved(
                            protocol::ServerRequestResolvedNotification {
                                thread_id: session.thread_id.clone(),
                                request_id,
                            },
                        ));
                    }
                }
                self.notify_all(resolved);
            }
            Some("system") => {
                if message.get("subtype").and_then(Value::as_str) == Some("init")
                    && let Some(version) =
                        message.get("claude_code_version").and_then(Value::as_str)
                {
                    self.claude_code_version = Some(version.to_string());
                }
            }
            Some("result") => {
                if message.get("parent_tool_use_id").is_none_or(Value::is_null) {
                    self.finish_turn(Some(&message), None);
                }
            }
            _ => {
                let notifications = self
                    .session
                    .as_mut()
                    .and_then(|session| session.turn.as_mut())
                    .map(|turn| turn.mapper.on_message(&message, now_ms(), &read_file))
                    .unwrap_or_default();
                self.notify_all(notifications);
            }
        }
    }

    fn on_control_response(&mut self, message: &Value) {
        let response = message.get("response").unwrap_or(&Value::Null);
        let request_id = response
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let failed = response.get("subtype").and_then(Value::as_str) == Some("error");
        let error_text = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let thread_id = session.thread_id.clone();
        let adopted = session.adopted;
        if session.init_request_id.as_deref() == Some(request_id) {
            session.init_request_id = None;
            if failed {
                self.warn(
                    adopted.then_some(thread_id.as_str()),
                    format!("{BACKEND_NAME} did not initialize: {error_text}"),
                );
            } else {
                let payload = response.get("response").unwrap_or(&Value::Null);
                if let Some(models) = payload.get("models").and_then(Value::as_array) {
                    self.reported_models = Some(models.clone());
                }
                if let Some(version) = payload.get("claude_code_version").and_then(Value::as_str) {
                    self.claude_code_version = Some(version.to_string());
                }
                if let Some(warning) = login_warning(payload.get("account")) {
                    self.warn(adopted.then_some(thread_id.as_str()), warning);
                }
            }
            self.init_settled = true;
            self.init_deadline = None;
            self.answer_model_lists();
        } else if failed {
            self.warn(
                Some(&thread_id),
                format!("{BACKEND_NAME} refused a request: {error_text}"),
            );
        }
    }
}

/// T3 Code reads the login from `initialize`'s `account`. With no email, token
/// or key source on the first-party API, the CLI is not logged in.
fn login_warning(account: Option<&Value>) -> Option<String> {
    let account = account?.as_object()?;
    let has = |key: &str| {
        account
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    };
    let first_party = account
        .get("apiProvider")
        .and_then(Value::as_str)
        .is_none_or(|provider| provider == "firstParty");
    let logged_in =
        has("email") || has("tokenSource") || has("apiKeySource") || has("organization");
    (first_party && !logged_in).then(|| {
        format!(
            "{BACKEND_NAME} does not report a login. Run `claude` in a terminal and use /login, then start this session again."
        )
    })
}

fn external_sandbox() -> protocol::SandboxPolicy {
    // Claude Code enforces its own permissions; Codex's sandbox does not apply.
    protocol::SandboxPolicy::ExternalSandbox {
        network_access: protocol::NetworkAccess::Enabled,
    }
}

fn control_id(session: &mut Session) -> String {
    session.next_control += 1;
    format!("codex-{}", session.next_control)
}

async fn write_line(stdin: &mut ChildStdin, message: &Value) -> io::Result<()> {
    let mut line = serde_json::to_string(message).map_err(io::Error::other)?;
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}
