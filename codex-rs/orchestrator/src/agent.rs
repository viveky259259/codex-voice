//! Engine-owned views of app-server threads and turns.
//!
//! These mirror a small slice of `codex-app-server-protocol` v2 so the engine
//! does not link the protocol crate. Each field names its protocol source; an
//! app-server-backed [`crate::AgentBackend`] fills them from the responses of
//! `thread/loaded/list`, `thread/list`, `thread/read`, and `thread/turns/list`.

use codex_voice_commands::AgentActivity;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;

/// One agent: an app-server thread on one machine.
///
/// Source: `v2::Thread` from `thread/read` (metadata only) for each id in
/// `thread/loaded/list`, plus `thread/list` for threads that are not loaded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    /// `Thread.id`.
    pub id: String,
    /// `Thread.name`, else `Thread.agent_nickname`, else the first words of
    /// `Thread.preview`. Never empty: the backend falls back to a short id.
    pub name: String,
    /// `Thread.status` (`ThreadStatus` plus `ThreadActiveFlag`s).
    pub status: AgentStatus,
    /// `Thread.cwd`.
    pub cwd: Option<PathBuf>,
    /// `Thread.preview`: usually the first user message. Screen only.
    pub preview: String,
    /// `Thread.updated_at` (Unix seconds).
    pub updated_at: Option<i64>,
    /// `Turn.started_at` (Unix seconds) of the `TurnStatus::InProgress` turn,
    /// from `thread/turns/list` (`limit: 1`, `sortDirection: desc`).
    pub turn_started_at: Option<i64>,
    /// The newest item of the newest turn (`Turn.items.last()`), when the
    /// backend loaded it. Drives the spoken "running cargo test".
    pub latest: Option<ActivityItem>,
}

/// What a thread is doing, from `v2::ThreadStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentStatus {
    /// `ThreadStatus::NotLoaded`: stored on disk, not in memory.
    NotLoaded,
    /// `ThreadStatus::Idle`: loaded, no turn in progress.
    Idle,
    /// `ThreadStatus::Active` with no waiting flag.
    Running,
    /// `ThreadStatus::Active` with `ThreadActiveFlag::WaitingOnApproval`.
    WaitingForApproval,
    /// `ThreadStatus::Active` with `ThreadActiveFlag::WaitingOnUserInput`.
    WaitingForInput,
    /// `ThreadStatus::SystemError`.
    Error,
}

impl AgentStatus {
    /// Whether a turn is in progress, so there is something to interrupt or
    /// steer with `turn/steer`.
    pub fn is_active(self) -> bool {
        match self {
            Self::Running | Self::WaitingForApproval | Self::WaitingForInput => true,
            Self::NotLoaded | Self::Idle | Self::Error => false,
        }
    }

    /// The `codex-voice-commands` view of this status.
    pub fn activity(self) -> AgentActivity {
        match self {
            Self::NotLoaded => AgentActivity::NotLoaded,
            Self::Idle => AgentActivity::Idle,
            Self::Running => AgentActivity::Working,
            Self::WaitingForApproval => AgentActivity::WaitingOnApproval,
            Self::WaitingForInput => AgentActivity::WaitingOnUserInput,
            Self::Error => AgentActivity::Error,
        }
    }
}

/// One thread item, reduced to what is worth showing or saying.
///
/// Source: `v2::ThreadItemPayload` in `Turn.items`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ActivityItem {
    /// `UserMessage { content }`, text inputs joined.
    UserMessage { text: String },
    /// `AgentMessage { text }`.
    AgentMessage { text: String },
    /// `Reasoning { summary }`, joined. Never spoken.
    Reasoning { summary: String },
    /// `Plan { text }`.
    Plan { text: String },
    /// `CommandExecution { command, status, exit_code, aggregated_output }`.
    /// The output is shown on screen (tail only) and never spoken.
    Command {
        command: String,
        status: ItemStatus,
        exit_code: Option<i32>,
        output: Option<String>,
    },
    /// `FileChange { changes, status }`: the changed paths only. Diffs are
    /// neither kept nor spoken.
    FileChange {
        paths: Vec<String>,
        status: ItemStatus,
    },
    /// `McpToolCall { tool, status }` and `DynamicToolCall { tool, status }`.
    ToolCall { tool: String, status: ItemStatus },
    /// Any other item kind (web search, image, review, ...).
    Other { kind: String },
}

/// Progress of a command, file change, or tool call. Source:
/// `CommandExecutionStatus`, `PatchApplyStatus`, and `McpToolCallStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

/// The answer to "what's build doing".
///
/// Source: `thread/read` (metadata) for `agent`, and `thread/turns/list`
/// (`limit: 1`, newest first, items loaded) or `thread/read` with
/// `includeTurns: true` for `recent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentReport {
    pub agent: AgentInfo,
    /// Items of the newest turn, oldest first.
    pub recent: Vec<ActivityItem>,
}

/// How a steer was delivered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SteerOutcome {
    /// `turn/steer` into the in-progress turn (`TurnSteerResponse.turn_id`).
    #[serde(rename_all = "camelCase")]
    Steered { turn_id: String },
    /// The thread was idle, so `turn/start` began a new turn
    /// (`TurnStartResponse.turn.id`).
    #[serde(rename_all = "camelCase")]
    Started { turn_id: String },
}

/// The result of `turn/interrupt`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InterruptOutcome {
    /// `turn/interrupt` was sent for this turn.
    #[serde(rename_all = "camelCase")]
    Interrupted { turn_id: String },
    /// No turn was in progress; nothing was sent.
    AlreadyIdle,
}

/// Why one backend call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    /// The machine could not be reached (connection refused, SSH failure).
    #[error("unreachable: {0}")]
    Unreachable(String),
    /// The connection to the machine is still being set up (for example an
    /// SSH handshake in progress). Ask again shortly.
    #[error("still connecting")]
    Connecting,
    /// The thread id is unknown on this machine.
    #[error("no agent with id {0}")]
    AgentNotFound(String),
    /// The app-server answered with an error.
    #[error("request failed: {0}")]
    Request(String),
}
