//! The per-machine interface the engine drives.

use crate::AgentInfo;
use crate::AgentReport;
use crate::BackendError;
use crate::InterruptOutcome;
use crate::SteerOutcome;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde::Serialize;

/// How the engine reaches a machine, for "list my machines" and the
/// remote-only rules (steer and stop always wait for "confirm").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MachineLink {
    /// This machine's own app server.
    Local,
    /// Another machine, connected (for example over SSH).
    Connected,
    /// Another machine; the first connection is being set up.
    Connecting,
    /// Another machine; not connected yet and no attempt is running.
    Disconnected,
    /// Another machine; the last connection attempt failed or the link
    /// dropped. The next call tries again.
    Unreachable,
}

impl MachineLink {
    /// Whether the machine is reached over the network rather than through
    /// this process's own app server.
    pub fn is_remote(self) -> bool {
        !matches!(self, Self::Local)
    }

    /// Whether commands can be sent right now.
    pub fn is_connected(self) -> bool {
        matches!(self, Self::Local | Self::Connected)
    }
}

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

    /// How this machine is reached right now. Must not block. Defaults to
    /// [`MachineLink::Local`]; remote backends report their connection state.
    fn link(&self) -> MachineLink {
        MachineLink::Local
    }

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
