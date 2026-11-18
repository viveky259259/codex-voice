//! The engine: snapshots, voice context, command execution, confirmation,
//! and "it" references.

use crate::AgentBackend;
use crate::AgentInfo;
use crate::AgentStatus;
use crate::InterruptOutcome;
use crate::Outcome;
use crate::OutcomeStatus;
use crate::PendingAction;
use crate::Snapshot;
use crate::SteerOutcome;
use crate::outcome::already_idle;
use crate::outcome::cancelled;
use crate::outcome::failed;
use crate::outcome::is_cancel_phrase;
use crate::outcome::no_backend;
use crate::pronoun::resolve_pronoun;
use crate::snapshot::bounded;
use crate::snapshot::collect;
use crate::speech::agent_ref;
use crate::speech::capitalize;
use crate::speech::list_screen;
use crate::speech::list_speech;
use crate::speech::report_screen;
use crate::speech::report_speech;
use crate::speech::spoken;
use codex_voice_commands::AgentRef;
use codex_voice_commands::Candidate;
use codex_voice_commands::ModelRef;
use codex_voice_commands::VoiceAction;
use codex_voice_commands::VoiceCommand;
use codex_voice_commands::VoiceContext;
use codex_voice_commands::completed_speech;
use codex_voice_commands::is_voice_confirmation;
use codex_voice_commands::parse_voice_command;
use codex_voice_commands::resolve_choice;
use codex_voice_commands::spoken_response;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// How long one machine may take to answer before it is reported as down.
pub const DEFAULT_BACKEND_TIMEOUT: Duration = Duration::from_secs(3);

/// An ambiguous command waiting for "the one on laptop".
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingChoice {
    action: VoiceAction,
    candidates: Vec<Candidate>,
}

type Clock = Box<dyn Fn() -> i64 + Send + Sync>;

fn system_clock() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Lists, reads, steers, and stops agents across machines.
///
/// One [`AgentBackend`] per machine. The engine keeps the last snapshot (for
/// name resolution), at most one pending destructive action or ambiguous
/// question, and the last agent the user asked about or acted on (for "it").
pub struct Orchestrator {
    backends: Vec<Box<dyn AgentBackend>>,
    timeout: Duration,
    known_machines: Vec<String>,
    known_models: Vec<ModelRef>,
    clock: Clock,
    snapshot: Option<Snapshot>,
    pending: Option<PendingAction>,
    pending_choice: Option<PendingChoice>,
    last_agent: Option<AgentRef>,
}

impl Orchestrator {
    pub fn new(backends: Vec<Box<dyn AgentBackend>>) -> Self {
        Self {
            backends,
            timeout: DEFAULT_BACKEND_TIMEOUT,
            known_machines: Vec::new(),
            known_models: Vec::new(),
            clock: Box::new(system_clock),
            snapshot: None,
            pending: None,
            pending_choice: None,
            last_agent: None,
        }
    }

    /// Per-backend timeout for every call (default 3 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Machines that can be attached to even without a backend, for example
    /// from `codex_tailnet::discover`.
    pub fn with_known_machines(mut self, machines: Vec<String>) -> Self {
        self.known_machines = machines;
        self
    }

    /// Models for "switch to opus".
    pub fn with_known_models(mut self, models: Vec<ModelRef>) -> Self {
        self.known_models = models;
        self
    }

    /// Unix-seconds clock used for "2 minutes". Defaults to the system clock.
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Ask every machine for its agents concurrently. A machine that errors
    /// or exceeds the timeout gets an error entry instead of failing the
    /// snapshot. The result is kept for [`Self::voice_context`].
    pub async fn snapshot(&mut self) -> &Snapshot {
        let snapshot = collect(&self.backends, self.timeout).await;
        self.snapshot.insert(snapshot)
    }

    /// The snapshot from the last [`Self::snapshot`] or list command.
    pub fn last_snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    /// The destructive action waiting for "confirm", if any.
    pub fn pending(&self) -> Option<&PendingAction> {
        self.pending.as_ref()
    }

    /// The agent "it" refers to: the last one asked about or acted on.
    pub fn last_agent(&self) -> Option<&AgentRef> {
        self.last_agent.as_ref()
    }

    /// The parser context: agents from the last snapshot, every backend's
    /// machine plus the known machines, and the known models.
    pub fn voice_context(&self) -> VoiceContext {
        let known_agents = self
            .snapshot
            .iter()
            .flat_map(Snapshot::agents)
            .map(|(machine, agent)| agent_ref(machine, agent))
            .collect();
        let mut known_machines = self.known_machines.clone();
        for backend in &self.backends {
            let machine = backend.machine();
            if !known_machines.iter().any(|known| known == machine) {
                known_machines.push(machine.to_string());
            }
        }
        VoiceContext {
            known_agents,
            known_machines,
            known_models: self.known_models.clone(),
        }
    }

    /// Parse a transcript, resolving "it" / "that agent" to the last agent
    /// when the plain parse finds no command.
    ///
    /// Precedence: while an approval prompt is open, approvals win, so a
    /// pronoun stop ("stop it") is not a command here and is left to Lane A's
    /// approval grammar. Otherwise "stop it" stops the last agent, after
    /// confirmation. A bare "stop" is never an agent command.
    pub fn parse(&self, transcript: &str, approval_prompt_open: bool) -> VoiceCommand {
        let command = parse_voice_command(transcript, &self.voice_context());
        if !matches!(command, VoiceCommand::None | VoiceCommand::NotFound { .. }) {
            return command;
        }
        let Some(last) = &self.last_agent else {
            return command;
        };
        match resolve_pronoun(transcript, last) {
            Some(VoiceCommand::StopAgent { .. }) if approval_prompt_open => VoiceCommand::None,
            Some(resolved) => resolved,
            None => command,
        }
    }

    /// Handle one final user transcript end to end: confirm or cancel a
    /// pending action, answer a pending "which one?", or parse and execute.
    ///
    /// Call it after Lane A's approval handler has declined the utterance.
    /// [`OutcomeStatus::PassThrough`] means the transcript goes to the model.
    pub async fn handle_utterance(
        &mut self,
        transcript: &str,
        approval_prompt_open: bool,
    ) -> Outcome {
        if let Some(pending) = self.pending.take() {
            if is_voice_confirmation(transcript) {
                if approval_prompt_open {
                    // "confirm" belongs to the open approval prompt.
                    self.pending = Some(pending);
                    return Outcome::pass_through();
                }
                return self.run(pending).await;
            }
            let cancelled = cancelled(&pending);
            if is_cancel_phrase(transcript) {
                return cancelled;
            }
            let mut next = self.handle_fresh(transcript, approval_prompt_open).await;
            next.spoken = spoken(&format!("{} {}", cancelled.spoken, next.spoken));
            return next;
        }
        if let Some(choice) = self.pending_choice.take() {
            if is_cancel_phrase(transcript) {
                return Outcome::new(
                    OutcomeStatus::Cancelled,
                    "Cancelled.",
                    "Cancelled.".to_string(),
                    VoiceCommand::None,
                );
            }
            let context = self.voice_context();
            if let Some(command) =
                resolve_choice(&choice.action, &choice.candidates, transcript, &context)
            {
                return self.execute(command).await;
            }
        }
        self.handle_fresh(transcript, approval_prompt_open).await
    }

    async fn handle_fresh(&mut self, transcript: &str, approval_prompt_open: bool) -> Outcome {
        if self.snapshot.is_none() {
            self.snapshot().await;
        }
        let command = self.parse(transcript, approval_prompt_open);
        self.execute(command).await
    }

    /// Run a parsed command. Steer and stop with `needs_confirmation` become
    /// the [`PendingAction`] and wait for [`Self::confirm`] or
    /// [`Self::cancel`]. Any earlier pending action or question is dropped.
    pub async fn execute(&mut self, command: VoiceCommand) -> Outcome {
        self.pending = None;
        self.pending_choice = None;
        match command {
            VoiceCommand::None => Outcome::pass_through(),
            VoiceCommand::ListAgents => {
                let now = (self.clock)();
                let snapshot = self.snapshot().await;
                let spoken_text = list_speech(snapshot, now);
                let screen = list_screen(snapshot, now);
                Outcome::new(OutcomeStatus::Done, &spoken_text, screen, command)
            }
            VoiceCommand::AgentStatus { agent } => self.status(agent).await,
            VoiceCommand::SteerAgent {
                agent,
                message,
                needs_confirmation,
            } => {
                self.last_agent = Some(agent.clone());
                let action = PendingAction::Steer { agent, message };
                if needs_confirmation {
                    self.await_confirmation(action)
                } else {
                    self.run(action).await
                }
            }
            VoiceCommand::StopAgent {
                agent,
                needs_confirmation,
            } => {
                self.last_agent = Some(agent.clone());
                if let Some(info) = self.known_info(&agent)
                    && !info.status.is_active()
                {
                    let command = VoiceCommand::StopAgent {
                        agent: agent.clone(),
                        needs_confirmation,
                    };
                    return already_idle(&agent, command);
                }
                let action = PendingAction::Stop { agent };
                if needs_confirmation {
                    self.await_confirmation(action)
                } else {
                    self.run(action).await
                }
            }
            VoiceCommand::Ambiguous {
                ref action,
                ref candidates,
                ..
            } => {
                let question = self.reply(&command);
                self.pending_choice = Some(PendingChoice {
                    action: action.clone(),
                    candidates: candidates.clone(),
                });
                let screen = candidates
                    .iter()
                    .enumerate()
                    .map(|(index, candidate)| format!("{}. {}", index + 1, candidate.label))
                    .collect::<Vec<_>>()
                    .join("\n");
                Outcome::new(OutcomeStatus::AwaitingChoice, &question, screen, command)
            }
            VoiceCommand::NotFound { .. } => {
                let reply = self.reply(&command);
                Outcome::new(OutcomeStatus::Failed, &reply, reply.clone(), command)
            }
            VoiceCommand::ListMachines => {
                let reply = self.reply(&command);
                let screen = self.voice_context().known_machines.join("\n");
                Outcome::new(OutcomeStatus::Done, &reply, screen, command)
            }
            VoiceCommand::SwitchModel { .. } | VoiceCommand::AttachMachine { .. } => {
                let reply = self.reply(&command);
                Outcome::new(OutcomeStatus::ForCaller, &reply, reply.clone(), command)
            }
        }
    }

    /// Run the pending action, if any.
    pub async fn confirm(&mut self) -> Outcome {
        match self.pending.take() {
            Some(pending) => self.run(pending).await,
            None => Outcome::new(
                OutcomeStatus::Failed,
                "Nothing to confirm.",
                "Nothing to confirm.".to_string(),
                VoiceCommand::None,
            ),
        }
    }

    /// Drop the pending action or question, if any.
    pub fn cancel(&mut self) -> Outcome {
        self.pending_choice = None;
        match self.pending.take() {
            Some(pending) => cancelled(&pending),
            None => Outcome::new(
                OutcomeStatus::Cancelled,
                "Nothing to cancel.",
                "Nothing to cancel.".to_string(),
                VoiceCommand::None,
            ),
        }
    }

    fn reply(&self, command: &VoiceCommand) -> String {
        spoken_response(command, &self.voice_context()).unwrap_or_default()
    }

    fn await_confirmation(&mut self, action: PendingAction) -> Outcome {
        let command = action.command();
        let question = self.reply(&command);
        let label = action.agent().spoken_label();
        let screen = match &action {
            PendingAction::Steer { message, .. } => {
                format!(
                    "Send to {label}:\n{message}\nSay \"confirm\" to send; anything else cancels."
                )
            }
            PendingAction::Stop { .. } => {
                format!("Interrupt {label}?\nSay \"confirm\" to stop it; anything else cancels.")
            }
        };
        self.pending = Some(action);
        Outcome::new(
            OutcomeStatus::AwaitingConfirmation,
            &question,
            screen,
            command,
        )
    }

    fn backend(&self, agent: &AgentRef) -> Option<&dyn AgentBackend> {
        let machine = agent.machine.as_deref()?;
        self.backends
            .iter()
            .find(|backend| backend.machine() == machine)
            .map(Box::as_ref)
    }

    fn known_info(&self, agent: &AgentRef) -> Option<&AgentInfo> {
        self.snapshot
            .as_ref()?
            .agent(agent.machine.as_deref()?, &agent.id)
    }

    /// Replace the snapshot's copy of an agent with a fresher one.
    fn remember(&mut self, machine: &str, fresh: &AgentInfo) {
        let Some(snapshot) = self.snapshot.as_mut() else {
            return;
        };
        let agents = snapshot
            .machines
            .iter_mut()
            .filter(|entry| entry.machine == machine)
            .filter_map(|entry| entry.agents.as_mut().ok())
            .flatten();
        for agent in agents.filter(|agent| agent.id == fresh.id) {
            *agent = fresh.clone();
        }
    }

    fn set_status(&mut self, agent: &AgentRef, status: AgentStatus) {
        if let Some(machine) = agent.machine.as_deref()
            && let Some(mut info) = self.known_info(agent).cloned()
        {
            info.status = status;
            let machine = machine.to_string();
            self.remember(&machine, &info);
        }
    }

    async fn status(&mut self, agent: AgentRef) -> Outcome {
        self.last_agent = Some(agent.clone());
        let command = VoiceCommand::AgentStatus {
            agent: agent.clone(),
        };
        let Some(backend) = self.backend(&agent) else {
            return no_backend(&agent, command);
        };
        let machine = backend.machine().to_string();
        match bounded(self.timeout, backend.agent_activity(&agent.id)).await {
            Ok(report) => {
                let now = (self.clock)();
                self.remember(&machine, &report.agent);
                let spoken_text = report_speech(&machine, &report, now);
                let screen = report_screen(&machine, &report, now);
                Outcome::new(OutcomeStatus::Done, &spoken_text, screen, command)
            }
            Err(error) => failed(&agent, error, command),
        }
    }

    async fn run(&mut self, action: PendingAction) -> Outcome {
        let command = action.command();
        let agent = action.agent().clone();
        self.last_agent = Some(agent.clone());
        let Some(backend) = self.backend(&agent) else {
            return no_backend(&agent, command);
        };
        let label = agent.spoken_label();
        match &action {
            PendingAction::Steer { message, .. } => {
                match bounded(self.timeout, backend.steer(&agent.id, message)).await {
                    Ok(SteerOutcome::Steered { turn_id }) => {
                        let reply =
                            completed_speech(&command, &self.voice_context()).unwrap_or_default();
                        let screen = format!("turn/steer {label} (turn {turn_id}):\n{message}");
                        Outcome::new(OutcomeStatus::Done, &reply, screen, command)
                    }
                    Ok(SteerOutcome::Started { turn_id }) => {
                        self.set_status(&agent, AgentStatus::Running);
                        let reply =
                            format!("{} was idle, so I started a new turn.", capitalize(&label));
                        let screen = format!("turn/start {label} (turn {turn_id}):\n{message}");
                        Outcome::new(OutcomeStatus::Done, &reply, screen, command)
                    }
                    Err(error) => failed(&agent, error, command),
                }
            }
            PendingAction::Stop { .. } => {
                match bounded(self.timeout, backend.interrupt(&agent.id)).await {
                    Ok(InterruptOutcome::Interrupted { turn_id }) => {
                        self.set_status(&agent, AgentStatus::Idle);
                        let reply =
                            completed_speech(&command, &self.voice_context()).unwrap_or_default();
                        let screen = format!("turn/interrupt {label} (turn {turn_id})");
                        Outcome::new(OutcomeStatus::Done, &reply, screen, command)
                    }
                    Ok(InterruptOutcome::AlreadyIdle) => {
                        self.set_status(&agent, AgentStatus::Idle);
                        already_idle(&agent, command)
                    }
                    Err(error) => failed(&agent, error, command),
                }
            }
        }
    }
}
