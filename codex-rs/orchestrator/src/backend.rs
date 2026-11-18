//! The per-machine interface the engine drives.

use crate::AgentInfo;
use crate::AgentReport;
use crate::BackendError;
use crate::InterruptOutcome;
use crate::SteerOutcome;
use futures::future::BoxFuture;

/// The future every [`AgentBackend`] method returns.
pub type BackendFuture<'a, T> = BoxFuture<'a, Result<T, BackendError>>;

/// One machine's agents. Each method mirrors app-server RPCs; see the README
/// ("TUI integration") for the exact calls an app-server-backed backend makes.
///
/// Methods return boxed futures so backends can live behind
/// `Box<dyn AgentBackend>`. The engine applies its own timeout to every call,
/// so implementations need not.
pub trait AgentBackend: Send + Sync {
    /// The machine name, for example "desktop" or the `ssh://` host. Used in
    /// speech ("build on desktop") and to route commands.
    fn machine(&self) -> &str;

    /// Every agent on this machine: `thread/loaded/list`, then `thread/read`
    /// for each id, plus recent unloaded threads from `thread/list`.
    fn list_agents(&self) -> BackendFuture<'_, Vec<AgentInfo>>;

    /// What one agent is doing: `thread/read` (metadata) plus the newest turn
    /// from `thread/turns/list`.
    fn agent_activity<'a>(&'a self, id: &'a str) -> BackendFuture<'a, AgentReport>;

    /// Send `message` into the agent's in-progress turn with `turn/steer`
    /// (`expectedTurnId` = that turn), or start a turn with `turn/start` when
    /// it is idle.
    fn steer<'a>(&'a self, id: &'a str, message: &'a str) -> BackendFuture<'a, SteerOutcome>;

    /// Interrupt the agent's in-progress turn with `turn/interrupt`. Returns
    /// [`InterruptOutcome::AlreadyIdle`] when no turn is running.
    fn interrupt<'a>(&'a self, id: &'a str) -> BackendFuture<'a, InterruptOutcome>;
}
