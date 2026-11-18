//! The voice orchestrator's view of one machine's app server.
//!
//! `AppServerAgentBackend` implements `codex_orchestrator::AgentBackend` over
//! any [`AppServerTransport`]: this machine's app-server request handle, or a
//! remote machine's SSH link (`voice_command_remote`). The same requests drive
//! both, on a background task:
//! - `list_agents`: `thread/loaded/list`, then `thread/read` per thread, plus
//!   the newest turn (`thread/turns/list`) of each active one;
//! - `agent_activity`: `thread/read` and the newest turn with its items;
//! - `steer`: `turn/steer` into the in-progress turn, or `turn/start` when idle;
//! - `interrupt`: `turn/interrupt`, as the agents overview's stop does.
//!
//! The mapping from protocol types to the engine's types is kept in pure
//! functions so it can be tested without a server.

use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::RequestResult;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::DynamicToolCallStatus;
use codex_app_server_protocol::McpToolCallStatus;
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::TurnSteerParams;
use codex_app_server_protocol::TurnSteerResponse;
use codex_app_server_protocol::UserInput;
use codex_orchestrator::ActivityItem;
use codex_orchestrator::AgentBackend;
use codex_orchestrator::AgentInfo;
use codex_orchestrator::AgentReport;
use codex_orchestrator::AgentStatus;
use codex_orchestrator::BackendError;
use codex_orchestrator::BackendFuture;
use codex_orchestrator::InterruptOutcome;
use codex_orchestrator::ItemStatus;
use codex_orchestrator::MachineLink;
use codex_orchestrator::SteerOutcome;
use codex_protocol::openai_models::ModelPreset;
use codex_voice_commands::ModelRef;
use futures::FutureExt;
use futures::future::BoxFuture;
use uuid::Uuid;

/// Most `thread/loaded/list` pages followed for one listing.
const MAX_LOADED_LIST_PAGES: usize = 10;
/// Words of a thread's first message used as its name when it has none.
const PREVIEW_NAME_WORDS: usize = 4;

/// Sends one app-server request and returns its JSON-RPC result.
///
/// The backend's request mapping is the same for every machine; only this
/// differs. Transport failures are reported as [`BackendError::Unreachable`],
/// or [`BackendError::Connecting`] while a remote link is still being set up.
pub(crate) trait AppServerTransport: Send + Sync {
    fn send(&self, request: ClientRequest) -> BoxFuture<'_, Result<RequestResult, BackendError>>;

    /// How the machine is reached right now. Must not block.
    fn link(&self) -> MachineLink {
        MachineLink::Local
    }

    /// Listen for approval requests from these agents, after a snapshot
    /// listed them. Must not block. This machine's threads already reach the
    /// TUI's own approval flow, so the default does nothing.
    fn watch_threads(&self, _agents: &[AgentInfo]) {}
}

/// This machine: the TUI's own app-server session.
impl AppServerTransport for AppServerRequestHandle {
    fn send(&self, request: ClientRequest) -> BoxFuture<'_, Result<RequestResult, BackendError>> {
        async move {
            let method = request.method_name();
            self.request(request)
                .await
                .map_err(|error| BackendError::Unreachable(format!("{method}: {error}")))
        }
        .boxed()
    }
}

/// One machine's agents, read and driven through its app server.
pub(crate) struct AppServerAgentBackend<T = AppServerRequestHandle> {
    machine: String,
    transport: T,
}

impl<T: AppServerTransport> AppServerAgentBackend<T> {
    pub(crate) fn new(machine: String, transport: T) -> Self {
        Self { machine, transport }
    }

    async fn request<R: serde::de::DeserializeOwned>(
        &self,
        thread_id: &str,
        request: impl FnOnce(RequestId) -> ClientRequest,
    ) -> Result<R, BackendError> {
        let request_id = RequestId::String(format!("voice-command-{}", Uuid::new_v4()));
        let request = request(request_id);
        let method = request.method_name();
        let result = self.transport.send(request).await?;
        decode_result(method, result).map_err(|error| backend_error(thread_id, error))
    }

    async fn read_thread(&self, id: &str, include_turns: bool) -> Result<Thread, BackendError> {
        let response: ThreadReadResponse = self
            .request(id, |request_id| ClientRequest::ThreadRead {
                request_id,
                params: ThreadReadParams {
                    thread_id: id.to_string(),
                    include_turns,
                },
            })
            .await?;
        Ok(response.thread)
    }

    /// The newest turn, with its items when `with_items` is set. Servers
    /// without `thread/turns/list` fall back to the full `thread/read`.
    async fn newest_turn(&self, id: &str, with_items: bool) -> Result<Option<Turn>, BackendError> {
        let page: Result<ThreadTurnsListResponse, BackendError> = self
            .request(id, |request_id| ClientRequest::ThreadTurnsList {
                request_id,
                params: newest_turn_params(id, with_items),
            })
            .await;
        match page {
            Ok(page) => Ok(page.data.into_iter().next()),
            Err(BackendError::Request(_)) => {
                let thread = self.read_thread(id, /*include_turns*/ true).await?;
                Ok(thread.turns.into_iter().last())
            }
            Err(error) => Err(error),
        }
    }

    async fn in_progress_turn_id(&self, id: &str) -> Result<Option<String>, BackendError> {
        let turn = self.newest_turn(id, /*with_items*/ false).await?;
        Ok(turn
            .filter(|turn| turn.status == TurnStatus::InProgress)
            .map(|turn| turn.id))
    }

    async fn loaded_thread_ids(&self) -> Result<Vec<String>, BackendError> {
        let mut ids = Vec::new();
        let mut cursor = None;
        for _ in 0..MAX_LOADED_LIST_PAGES {
            let page: ThreadLoadedListResponse = self
                .request("", |request_id| ClientRequest::ThreadLoadedList {
                    request_id,
                    params: ThreadLoadedListParams {
                        cursor: cursor.take(),
                        limit: None,
                    },
                })
                .await?;
            ids.extend(page.data);
            match page.next_cursor.filter(|next| !next.is_empty()) {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(ids)
    }

    async fn agent(&self, id: String) -> Result<Option<AgentInfo>, BackendError> {
        let thread = match self.read_thread(&id, /*include_turns*/ false).await {
            Ok(thread) => thread,
            // Unloaded between the list and the read.
            Err(BackendError::AgentNotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        if thread.ephemeral {
            return Ok(None);
        }
        let newest = if agent_status(&thread.status).is_active() {
            self.newest_turn(&id, /*with_items*/ true)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        Ok(Some(agent_info(&thread, newest.as_ref())))
    }
}

impl<T: AppServerTransport> AgentBackend for AppServerAgentBackend<T> {
    fn machine(&self) -> &str {
        &self.machine
    }

    fn link(&self) -> MachineLink {
        self.transport.link()
    }

    fn list_agents(&self) -> BackendFuture<'_, Vec<AgentInfo>> {
        async move {
            let ids = self.loaded_thread_ids().await?;
            let agents = futures::future::join_all(ids.into_iter().map(|id| self.agent(id))).await;
            let mut found = Vec::new();
            for agent in agents {
                found.extend(agent?);
            }
            self.transport.watch_threads(&found);
            Ok(found)
        }
        .boxed()
    }

    fn agent_activity<'a>(&'a self, id: &'a str) -> BackendFuture<'a, AgentReport> {
        async move {
            let thread = self.read_thread(id, /*include_turns*/ false).await?;
            let newest = self.newest_turn(id, /*with_items*/ true).await?;
            Ok(agent_report(&thread, newest.as_ref()))
        }
        .boxed()
    }

    fn steer<'a>(&'a self, id: &'a str, message: &'a str) -> BackendFuture<'a, SteerOutcome> {
        async move {
            let input = vec![UserInput::Text {
                text: message.to_string(),
                text_elements: Vec::new(),
            }];
            match self.in_progress_turn_id(id).await? {
                Some(turn_id) => {
                    let response: TurnSteerResponse = self
                        .request(id, |request_id| ClientRequest::TurnSteer {
                            request_id,
                            params: steer_params(id, turn_id, input),
                        })
                        .await?;
                    Ok(SteerOutcome::Steered {
                        turn_id: response.turn_id,
                    })
                }
                None => {
                    let response: TurnStartResponse = self
                        .request(id, |request_id| ClientRequest::TurnStart {
                            request_id,
                            params: start_params(id, input),
                        })
                        .await?;
                    Ok(SteerOutcome::Started {
                        turn_id: response.turn.id,
                    })
                }
            }
        }
        .boxed()
    }

    fn interrupt<'a>(&'a self, id: &'a str) -> BackendFuture<'a, InterruptOutcome> {
        async move {
            let Some(turn_id) = self.in_progress_turn_id(id).await? else {
                return Ok(InterruptOutcome::AlreadyIdle);
            };
            let _: TurnInterruptResponse = self
                .request(id, |request_id| ClientRequest::TurnInterrupt {
                    request_id,
                    params: TurnInterruptParams {
                        thread_id: id.to_string(),
                        turn_id: turn_id.clone(),
                    },
                })
                .await?;
            Ok(InterruptOutcome::Interrupted { turn_id })
        }
        .boxed()
    }
}

/// `thread/turns/list` for the newest turn only.
pub(crate) fn newest_turn_params(thread_id: &str, with_items: bool) -> ThreadTurnsListParams {
    ThreadTurnsListParams {
        thread_id: thread_id.to_string(),
        cursor: None,
        limit: Some(1),
        sort_direction: Some(SortDirection::Desc),
        items_view: Some(if with_items {
            TurnItemsView::Full
        } else {
            TurnItemsView::NotLoaded
        }),
    }
}

/// `turn/steer` into the running turn, with a fresh client message id.
pub(crate) fn steer_params(
    thread_id: &str,
    turn_id: String,
    input: Vec<UserInput>,
) -> TurnSteerParams {
    TurnSteerParams {
        thread_id: thread_id.to_string(),
        client_user_message_id: Some(Uuid::new_v4().to_string()),
        input,
        responsesapi_client_metadata: None,
        responsesapi_client_metadata_patch: None,
        additional_context: None,
        expected_turn_id: turn_id,
    }
}

/// `turn/start` with the thread's stored settings, like a typed message.
pub(crate) fn start_params(thread_id: &str, input: Vec<UserInput>) -> TurnStartParams {
    TurnStartParams {
        thread_id: thread_id.to_string(),
        client_user_message_id: Some(Uuid::new_v4().to_string()),
        input,
        ..TurnStartParams::default()
    }
}

/// A JSON-RPC result decoded into the response type, as
/// `AppServerRequestHandle::request_typed` does.
pub(crate) fn decode_result<R: serde::de::DeserializeOwned>(
    method: &str,
    result: RequestResult,
) -> Result<R, TypedRequestError> {
    let value = result.map_err(|source| TypedRequestError::Server {
        method: method.to_string(),
        source,
    })?;
    serde_json::from_value(value).map_err(|source| TypedRequestError::Deserialize {
        method: method.to_string(),
        source,
    })
}

/// Transport failures mean the machine is down; an unknown thread id means
/// the agent is gone; anything else is a failed request.
pub(crate) fn backend_error(thread_id: &str, error: TypedRequestError) -> BackendError {
    match error {
        TypedRequestError::Transport { .. } => BackendError::Unreachable(error.to_string()),
        TypedRequestError::Server { ref source, .. }
            if source.message.starts_with("thread not found")
                || source.message.starts_with("invalid thread id") =>
        {
            BackendError::AgentNotFound(thread_id.to_string())
        }
        error => BackendError::Request(error.to_string()),
    }
}

pub(crate) fn agent_status(status: &ThreadStatus) -> AgentStatus {
    match status {
        ThreadStatus::NotLoaded => AgentStatus::NotLoaded,
        ThreadStatus::Idle => AgentStatus::Idle,
        ThreadStatus::SystemError => AgentStatus::Error,
        ThreadStatus::Active { active_flags } => {
            if active_flags.contains(&ThreadActiveFlag::WaitingOnApproval) {
                AgentStatus::WaitingForApproval
            } else if active_flags.contains(&ThreadActiveFlag::WaitingOnUserInput) {
                AgentStatus::WaitingForInput
            } else {
                AgentStatus::Running
            }
        }
    }
}

/// The thread's title, else its sub-agent nickname, else the first words of
/// its first message, else a short id.
pub(crate) fn agent_name(thread: &Thread) -> String {
    let named = [thread.name.as_deref(), thread.agent_nickname.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| !name.is_empty());
    if let Some(name) = named {
        return name.to_string();
    }
    let preview = thread
        .preview
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            line.split_whitespace()
                .take(PREVIEW_NAME_WORDS)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    if !preview.is_empty() {
        return preview;
    }
    let tail: String = thread
        .id
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("agent {tail}")
}

pub(crate) fn agent_info(thread: &Thread, newest_turn: Option<&Turn>) -> AgentInfo {
    let running_turn = newest_turn.filter(|turn| turn.status == TurnStatus::InProgress);
    AgentInfo {
        id: thread.id.clone(),
        name: agent_name(thread),
        status: agent_status(&thread.status),
        cwd: Some(thread.cwd.to_path_buf()),
        preview: thread.preview.clone(),
        updated_at: Some(thread.updated_at),
        turn_started_at: running_turn.and_then(|turn| turn.started_at),
        latest: newest_turn
            .and_then(|turn| turn.items.last())
            .map(|item| activity_item(&item.payload)),
    }
}

pub(crate) fn agent_report(thread: &Thread, newest_turn: Option<&Turn>) -> AgentReport {
    AgentReport {
        agent: agent_info(thread, newest_turn),
        recent: newest_turn
            .map(|turn| {
                turn.items
                    .iter()
                    .map(|item| activity_item(&item.payload))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn command_status(status: &CommandExecutionStatus) -> ItemStatus {
    match status {
        CommandExecutionStatus::InProgress => ItemStatus::InProgress,
        CommandExecutionStatus::Completed => ItemStatus::Completed,
        CommandExecutionStatus::Failed => ItemStatus::Failed,
        CommandExecutionStatus::Declined => ItemStatus::Declined,
    }
}

fn patch_status(status: &PatchApplyStatus) -> ItemStatus {
    match status {
        PatchApplyStatus::InProgress => ItemStatus::InProgress,
        PatchApplyStatus::Completed => ItemStatus::Completed,
        PatchApplyStatus::Failed => ItemStatus::Failed,
        PatchApplyStatus::Declined => ItemStatus::Declined,
    }
}

fn mcp_status(status: &McpToolCallStatus) -> ItemStatus {
    match status {
        McpToolCallStatus::InProgress => ItemStatus::InProgress,
        McpToolCallStatus::Completed => ItemStatus::Completed,
        McpToolCallStatus::Failed => ItemStatus::Failed,
    }
}

fn dynamic_status(status: &DynamicToolCallStatus) -> ItemStatus {
    match status {
        DynamicToolCallStatus::InProgress => ItemStatus::InProgress,
        DynamicToolCallStatus::Completed => ItemStatus::Completed,
        DynamicToolCallStatus::Failed => ItemStatus::Failed,
    }
}

/// One thread item, reduced to what is worth showing or saying. Diffs and
/// tool arguments are dropped.
pub(crate) fn activity_item(payload: &ThreadItemPayload) -> ActivityItem {
    match payload {
        ThreadItemPayload::UserMessage { content, .. } => ActivityItem::UserMessage {
            text: content
                .iter()
                .filter_map(|input| match input {
                    UserInput::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        },
        ThreadItemPayload::AgentMessage { text, .. } => {
            ActivityItem::AgentMessage { text: text.clone() }
        }
        ThreadItemPayload::Reasoning { summary, .. } => ActivityItem::Reasoning {
            summary: summary.join(" "),
        },
        ThreadItemPayload::Plan { text, .. } => ActivityItem::Plan { text: text.clone() },
        ThreadItemPayload::CommandExecution {
            command,
            status,
            exit_code,
            aggregated_output,
            ..
        } => ActivityItem::Command {
            command: command.clone(),
            status: command_status(status),
            exit_code: *exit_code,
            output: aggregated_output.clone(),
        },
        ThreadItemPayload::FileChange {
            changes, status, ..
        } => ActivityItem::FileChange {
            paths: changes.iter().map(|change| change.path.clone()).collect(),
            status: patch_status(status),
        },
        ThreadItemPayload::McpToolCall { tool, status, .. } => ActivityItem::ToolCall {
            tool: tool.clone(),
            status: mcp_status(status),
        },
        ThreadItemPayload::DynamicToolCall { tool, status, .. } => ActivityItem::ToolCall {
            tool: tool.clone(),
            status: dynamic_status(status),
        },
        other => ActivityItem::Other {
            kind: serde_json::to_value(other)
                .ok()
                .and_then(|value| value.get("type")?.as_str().map(str::to_string))
                .unwrap_or_else(|| "item".to_string()),
        },
    }
}

/// The models "switch to opus" can pick: every catalog model by slug, also
/// spoken by its display name.
pub(crate) fn voice_models(presets: &[ModelPreset]) -> Vec<ModelRef> {
    let mut models: Vec<ModelRef> = Vec::new();
    for preset in presets {
        if models.iter().any(|model| model.slug == preset.model) {
            continue;
        }
        let display_name = preset.display_name.trim();
        let spoken_aliases = if display_name.is_empty() || display_name == preset.model {
            Vec::new()
        } else {
            vec![display_name.to_string()]
        };
        models.push(ModelRef {
            slug: preset.model.clone(),
            spoken_aliases,
        });
    }
    models
}

/// This machine's name in speech: the hostname without its domain.
pub(crate) fn local_machine_name() -> String {
    short_host_name(&gethostname::gethostname().to_string_lossy())
}

pub(crate) fn short_host_name(hostname: &str) -> String {
    hostname
        .trim()
        .split('.')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("this machine")
        .to_string()
}

#[cfg(test)]
#[path = "voice_command_backend_tests.rs"]
pub(crate) mod tests;
