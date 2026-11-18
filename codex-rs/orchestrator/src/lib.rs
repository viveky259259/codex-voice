//! Voice orchestration of Codex agents across machines.
//!
//! One [`AgentBackend`] per machine lists its agents (app-server threads),
//! reads what one is doing, steers it, and interrupts it. The
//! [`Orchestrator`] fans out to every backend with a per-machine timeout,
//! builds the [`codex_voice_commands::VoiceContext`] the parser needs, and
//! executes parsed [`codex_voice_commands::VoiceCommand`]s, returning an
//! [`Outcome`] with a short spoken reply and a longer screen text.
//!
//! Steer and stop wait for "confirm" as a [`PendingAction`]. "It" and "that
//! agent" refer to the last agent the user asked about or acted on.

mod agent;
mod backend;
mod orchestrator;
mod outcome;
mod pronoun;
mod snapshot;
mod speech;
#[cfg(test)]
mod test_support;

pub use agent::ActivityItem;
pub use agent::AgentInfo;
pub use agent::AgentReport;
pub use agent::AgentStatus;
pub use agent::BackendError;
pub use agent::InterruptOutcome;
pub use agent::ItemStatus;
pub use agent::SteerOutcome;
pub use backend::AgentBackend;
pub use backend::BackendFuture;
pub use orchestrator::DEFAULT_BACKEND_TIMEOUT;
pub use orchestrator::Orchestrator;
pub use outcome::Outcome;
pub use outcome::OutcomeStatus;
pub use outcome::PendingAction;
pub use snapshot::MachineError;
pub use snapshot::MachineSnapshot;
pub use snapshot::Snapshot;
pub use speech::activity_phrase;
pub use speech::agent_line;
pub use speech::agent_ref;
pub use speech::elapsed_phrase;
pub use speech::spoken_machine;
pub use speech::spoken_message;

#[cfg(test)]
#[path = "orchestrator_tests.rs"]
mod orchestrator_tests;
