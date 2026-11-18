//! An in-memory [`AgentBackend`] for tests.

use crate::ActivityItem;
use crate::AgentBackend;
use crate::AgentInfo;
use crate::AgentReport;
use crate::AgentStatus;
use crate::BackendError;
use crate::BackendFuture;
use crate::InterruptOutcome;
use crate::SteerOutcome;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Notify;

/// A request the fake received, for assertions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    Steer { id: String, message: String },
    Interrupt { id: String },
}

/// How the fake answers `list_agents`.
#[derive(Clone)]
pub(crate) enum Behavior {
    Answer,
    /// Never answers.
    Hang,
    /// Answers only after another backend calls [`Behavior::Signal`].
    WaitFor(Arc<Notify>),
    /// Wakes a [`Behavior::WaitFor`] backend, then answers.
    Signal(Arc<Notify>),
}

#[derive(Default)]
pub(crate) struct FakeState {
    pub(crate) agents: Vec<AgentInfo>,
    pub(crate) recent: HashMap<String, Vec<ActivityItem>>,
    pub(crate) calls: Vec<Call>,
}

pub(crate) struct FakeBackend {
    machine: String,
    behavior: Behavior,
    pub(crate) state: Arc<Mutex<FakeState>>,
}

impl FakeBackend {
    pub(crate) fn new(machine: &str, agents: Vec<AgentInfo>) -> Self {
        Self {
            machine: machine.to_string(),
            behavior: Behavior::Answer,
            state: Arc::new(Mutex::new(FakeState {
                agents,
                ..FakeState::default()
            })),
        }
    }

    pub(crate) fn with_behavior(mut self, behavior: Behavior) -> Self {
        self.behavior = behavior;
        self
    }

    pub(crate) fn with_recent(self, id: &str, recent: Vec<ActivityItem>) -> Self {
        self.state
            .lock()
            .unwrap()
            .recent
            .insert(id.to_string(), recent);
        self
    }

    fn agent(&self, id: &str) -> Result<AgentInfo, BackendError> {
        self.state
            .lock()
            .unwrap()
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .cloned()
            .ok_or_else(|| BackendError::AgentNotFound(id.to_string()))
    }

    fn set_status(&self, id: &str, status: AgentStatus) {
        let mut state = self.state.lock().unwrap();
        for agent in state.agents.iter_mut().filter(|agent| agent.id == id) {
            agent.status = status;
        }
    }
}

pub(crate) fn agent(id: &str, name: &str, status: AgentStatus) -> AgentInfo {
    AgentInfo {
        id: id.to_string(),
        name: name.to_string(),
        status,
        cwd: None,
        preview: String::new(),
        updated_at: None,
        turn_started_at: None,
        latest: None,
    }
}

impl AgentBackend for FakeBackend {
    fn machine(&self) -> &str {
        &self.machine
    }

    fn list_agents(&self) -> BackendFuture<'_, Vec<AgentInfo>> {
        Box::pin(async move {
            match &self.behavior {
                Behavior::Answer => {}
                Behavior::Hang => std::future::pending::<()>().await,
                Behavior::WaitFor(notify) => notify.notified().await,
                Behavior::Signal(notify) => notify.notify_one(),
            }
            Ok(self.state.lock().unwrap().agents.clone())
        })
    }

    fn agent_activity<'a>(&'a self, id: &'a str) -> BackendFuture<'a, AgentReport> {
        Box::pin(async move {
            let agent = self.agent(id)?;
            let recent = self
                .state
                .lock()
                .unwrap()
                .recent
                .get(id)
                .cloned()
                .unwrap_or_default();
            Ok(AgentReport { agent, recent })
        })
    }

    fn steer<'a>(&'a self, id: &'a str, message: &'a str) -> BackendFuture<'a, SteerOutcome> {
        Box::pin(async move {
            let agent = self.agent(id)?;
            self.state.lock().unwrap().calls.push(Call::Steer {
                id: id.to_string(),
                message: message.to_string(),
            });
            if agent.status.is_active() {
                Ok(SteerOutcome::Steered {
                    turn_id: format!("{id}-turn"),
                })
            } else {
                self.set_status(id, AgentStatus::Running);
                Ok(SteerOutcome::Started {
                    turn_id: format!("{id}-new-turn"),
                })
            }
        })
    }

    fn interrupt<'a>(&'a self, id: &'a str) -> BackendFuture<'a, InterruptOutcome> {
        Box::pin(async move {
            let agent = self.agent(id)?;
            self.state
                .lock()
                .unwrap()
                .calls
                .push(Call::Interrupt { id: id.to_string() });
            if agent.status.is_active() {
                self.set_status(id, AgentStatus::Idle);
                Ok(InterruptOutcome::Interrupted {
                    turn_id: format!("{id}-turn"),
                })
            } else {
                Ok(InterruptOutcome::AlreadyIdle)
            }
        })
    }
}
