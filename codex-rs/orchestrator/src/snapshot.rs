//! Concurrent fan-out to every machine, with a per-machine timeout.

use crate::AgentBackend;
use crate::AgentInfo;
use crate::BackendError;
use serde::Deserialize;
use serde::Serialize;
use std::future::Future;
use std::time::Duration;

/// Every machine's agents at one moment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// One entry per backend, in backend order.
    pub machines: Vec<MachineSnapshot>,
}

impl Snapshot {
    /// Every reachable agent with its machine name.
    pub fn agents(&self) -> impl Iterator<Item = (&str, &AgentInfo)> {
        self.machines.iter().flat_map(|machine| {
            machine
                .agents
                .iter()
                .flatten()
                .map(|agent| (machine.machine.as_str(), agent))
        })
    }

    /// The agent `id` on `machine`, if that machine answered.
    pub fn agent(&self, machine: &str, id: &str) -> Option<&AgentInfo> {
        self.agents()
            .find(|(name, agent)| *name == machine && agent.id == id)
            .map(|(_, agent)| agent)
    }

    /// Machines that did not answer, with the reason.
    pub fn failures(&self) -> impl Iterator<Item = (&str, &MachineError)> {
        self.machines.iter().filter_map(|machine| {
            machine
                .agents
                .as_ref()
                .err()
                .map(|error| (machine.machine.as_str(), error))
        })
    }
}

/// One machine's answer to `list_agents`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineSnapshot {
    pub machine: String,
    pub agents: Result<Vec<AgentInfo>, MachineError>,
}

/// Why a machine is missing from a snapshot or a command failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum MachineError {
    /// No answer within the per-machine timeout.
    #[error("timed out after {}ms", .after.as_millis())]
    Timeout { after: Duration },
    /// The connection to the machine is still being set up.
    #[error("connecting")]
    Connecting,
    /// The backend returned an error.
    #[error("{message}")]
    Backend { message: String },
}

impl From<BackendError> for MachineError {
    fn from(error: BackendError) -> Self {
        match error {
            BackendError::Connecting => Self::Connecting,
            error => Self::Backend {
                message: error.to_string(),
            },
        }
    }
}

/// Run one backend call under `timeout`.
pub(crate) async fn bounded<T>(
    timeout: Duration,
    call: impl Future<Output = Result<T, BackendError>>,
) -> Result<T, CallError> {
    match tokio::time::timeout(timeout, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(CallError::Backend(error)),
        Err(_) => Err(CallError::Timeout { after: timeout }),
    }
}

/// A failed backend call, before it is reported per machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CallError {
    Timeout { after: Duration },
    Backend(BackendError),
}

impl From<CallError> for MachineError {
    fn from(error: CallError) -> Self {
        match error {
            CallError::Timeout { after } => Self::Timeout { after },
            CallError::Backend(error) => error.into(),
        }
    }
}

/// Ask every backend for its agents at once. A slow or failing machine
/// becomes an error entry; it never fails the snapshot.
pub(crate) async fn collect(backends: &[Box<dyn AgentBackend>], timeout: Duration) -> Snapshot {
    let calls = backends.iter().map(|backend| async move {
        MachineSnapshot {
            machine: backend.machine().to_string(),
            agents: bounded(timeout, backend.list_agents())
                .await
                .map_err(MachineError::from),
        }
    });
    Snapshot {
        machines: futures::future::join_all(calls).await,
    }
}
