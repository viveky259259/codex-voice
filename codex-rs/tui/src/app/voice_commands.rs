//! Voice commands in the app: the orchestrator that runs them.
//!
//! The voice widget decides which transcripts are commands and sends them as
//! `AppEvent::VoiceCommand`. The app owns one `codex_orchestrator::Orchestrator`
//! on a background task, so app-server calls never block the UI loop. Requests
//! are handled in order; each answer comes back as
//! `AppEvent::VoiceCommandResult` and is spoken by the voice widget. Model
//! switches go through the `/model` picker's apply event. Attaching to another
//! machine is not done at runtime yet: the answer says which command to run.
//!
//! Besides this machine, the orchestrator drives every machine listed in
//! `[realtime] voice_machines`, over SSH (`voice_command_remote`). Their
//! connections start in the background when the orchestrator starts (the
//! first voice command or voice start), so a slow SSH handshake is reported
//! as "connecting" instead of holding up this machine's answers. Those links
//! also bring the remote agents' approval requests into this TUI's approval
//! overlay (`voice_remote_approvals`).

use super::App;
use super::voice_command_backend::AppServerAgentBackend;
use super::voice_command_backend::local_machine_name;
use super::voice_command_backend::voice_models;
use super::voice_command_remote::VoiceMachineTarget;
use super::voice_command_remote::remote_backends;
use super::voice_command_remote::resolve_voice_machines;
use super::voice_remote_approvals::RemoteApprovals;
use crate::app_event::AppEvent;
use crate::app_event::VoiceCommandResult;
use crate::app_event_sender::AppEventSender;
use crate::app_server_session::AppServerSession;
use crate::backend_id::BackendId;
use codex_config::config_toml::VoiceMachine;
use codex_orchestrator::AgentBackend;
use codex_orchestrator::Orchestrator;
use codex_orchestrator::OutcomeStatus;
use codex_protocol::ThreadId;
use codex_voice_commands::ModelRef;
use codex_voice_commands::VoiceCommand;
use codex_voice_commands::is_voice_confirmation;
use std::time::Duration;
use tokio::sync::mpsc;

/// How long tailnet and SSH-config discovery may take when voice starts.
const MACHINE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

/// Where the names for "list my machines" come from.
#[derive(Clone, Debug, Default)]
pub(crate) enum MachineDiscovery {
    /// `codex_tailnet::discover`: Tailscale peers plus `~/.ssh/config` hosts.
    #[default]
    Tailnet,
    /// Fixed names (tests).
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(Vec<String>),
}

/// One request for the orchestrator task.
#[derive(Debug)]
pub(crate) enum VoiceCommandJob {
    /// Take a fresh agent snapshot (voice just started).
    Refresh,
    /// A transcript the widget kept from the model.
    Utterance {
        transcript: String,
        approval_prompt_open: bool,
    },
    /// Drop any pending confirmation or question.
    Cancel,
}

struct QueuedJob {
    thread_id: ThreadId,
    seq: u64,
    /// The current model catalog, for "switch to opus".
    models: Vec<ModelRef>,
    job: VoiceCommandJob,
}

/// The app's handle on the orchestrator task.
#[derive(Default)]
pub(crate) struct VoiceCommandRunner {
    jobs: Option<(BackendId, mpsc::UnboundedSender<QueuedJob>)>,
    pub(crate) machine_discovery: MachineDiscovery,
    /// Approval requests from agents on the configured remote machines.
    pub(crate) remote_approvals: RemoteApprovals,
}

impl VoiceCommandRunner {
    /// Queue a job, starting the orchestrator task for `app_server`'s backend
    /// when none runs for it yet.
    fn submit(
        &mut self,
        app_server: &AppServerSession,
        app_event_tx: &AppEventSender,
        voice_machines: &[VoiceMachine],
        job: QueuedJob,
    ) {
        let backend_id = app_server.backend_id();
        let running = self
            .jobs
            .as_ref()
            .is_some_and(|(owner, jobs)| *owner == backend_id && !jobs.is_closed());
        if !running {
            let local_machine = local_machine_name();
            let (remotes, warnings) = resolve_voice_machines(voice_machines, &local_machine);
            for warning in warnings {
                tracing::warn!("{warning}");
            }
            let backend = AppServerAgentBackend::new(local_machine, app_server.request_handle());
            let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
            tokio::spawn(run_orchestrator(
                Box::new(backend),
                remotes,
                self.machine_discovery.clone(),
                jobs_rx,
                app_event_tx.clone(),
            ));
            self.jobs = Some((backend_id, jobs_tx));
        }
        if let Some((_, jobs)) = self.jobs.as_ref()
            && jobs.send(job).is_err()
        {
            tracing::warn!("voice command orchestrator stopped");
        }
    }
}

async fn discover_machines(discovery: MachineDiscovery) -> Vec<String> {
    match discovery {
        MachineDiscovery::Fixed(names) => names,
        MachineDiscovery::Tailnet => {
            let ssh_config = codex_tailnet::SshConfigSource::from_home();
            let tailscale = codex_tailnet::TailscaleCli::from_env();
            let discovered = tokio::time::timeout(
                MACHINE_DISCOVERY_TIMEOUT,
                codex_tailnet::discover(&tailscale, ssh_config.as_ref()),
            )
            .await;
            match discovered {
                Ok(Ok(discovery)) => discovery
                    .machines
                    .into_iter()
                    .filter(|machine| !machine.is_self)
                    .map(|machine| machine.name)
                    .collect(),
                Ok(Err(error)) => {
                    tracing::debug!("voice commands: no machines discovered: {error}");
                    Vec::new()
                }
                Err(_) => {
                    tracing::debug!("voice commands: machine discovery timed out");
                    Vec::new()
                }
            }
        }
    }
}

/// What to say when the widget kept a transcript the orchestrator did not
/// take as a command: its parser state moved on in between.
fn pass_through_reply(transcript: &str) -> &'static str {
    if is_voice_confirmation(transcript) {
        "Nothing to confirm."
    } else {
        "I couldn't run that as a voice command."
    }
}

/// The engine for this machine plus the configured remote machines, whose
/// SSH connections start now, in the background. With `approvals`, the remote
/// machines' approval requests are forwarded there.
pub(crate) fn orchestrator_for(
    local: Box<dyn AgentBackend>,
    remotes: Vec<VoiceMachineTarget>,
    approvals: Option<&AppEventSender>,
) -> Orchestrator {
    let mut backends = vec![local];
    backends.extend(
        remote_backends(remotes, approvals)
            .into_iter()
            .map(|backend| Box::new(backend) as Box<dyn AgentBackend>),
    );
    Orchestrator::new(backends)
}

async fn run_orchestrator(
    local: Box<dyn AgentBackend>,
    remotes: Vec<VoiceMachineTarget>,
    discovery: MachineDiscovery,
    mut jobs: mpsc::UnboundedReceiver<QueuedJob>,
    app_event_tx: AppEventSender,
) {
    let mut engine = orchestrator_for(local, remotes, Some(&app_event_tx));
    engine.set_known_machines(discover_machines(discovery).await);
    while let Some(QueuedJob {
        thread_id,
        seq,
        models,
        job,
    }) = jobs.recv().await
    {
        engine.set_known_models(models);
        let mut spoken_override = None;
        let outcome = match job {
            VoiceCommandJob::Refresh => {
                engine.snapshot().await;
                None
            }
            VoiceCommandJob::Cancel => {
                engine.cancel();
                None
            }
            VoiceCommandJob::Utterance {
                transcript,
                approval_prompt_open,
            } => {
                let outcome = engine
                    .handle_utterance(&transcript, approval_prompt_open)
                    .await;
                if outcome.status == OutcomeStatus::PassThrough && outcome.spoken.is_empty() {
                    spoken_override = Some(pass_through_reply(&transcript).to_string());
                }
                Some(outcome)
            }
        };
        app_event_tx.send(AppEvent::VoiceCommandResult {
            thread_id,
            result: Box::new(VoiceCommandResult {
                seq,
                outcome,
                spoken_override,
                context: engine.voice_context(),
                last_agent: engine.last_agent().cloned(),
            }),
        });
    }
}

impl App {
    /// Queue a voice-command request from the voice widget.
    pub(super) fn submit_voice_command(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        seq: u64,
        job: VoiceCommandJob,
    ) {
        let models = voice_models(&self.model_catalog.models);
        self.voice_commands.submit(
            app_server,
            &self.app_event_tx,
            &self.config.realtime.voice_machines,
            QueuedJob {
                thread_id,
                seq,
                models,
                job,
            },
        );
    }

    /// Perform what the orchestrator leaves to the app, then hand the answer
    /// to the voice widget that asked.
    pub(super) fn handle_voice_command_result(
        &mut self,
        thread_id: ThreadId,
        mut result: Box<VoiceCommandResult>,
    ) {
        if let Some(outcome) = result.outcome.as_ref()
            && outcome.status == OutcomeStatus::ForCaller
        {
            match &outcome.command {
                VoiceCommand::SwitchModel { slug } => {
                    if self.chat_widget.thread_id() == Some(thread_id) {
                        // The `/model` picker's apply.
                        self.app_event_tx.send(AppEvent::ApplyModelSelection {
                            model: slug.clone(),
                            effort: None,
                        });
                    } else {
                        result.spoken_override =
                            Some("Open that conversation to switch its model.".to_string());
                    }
                }
                VoiceCommand::AttachMachine { machine } => {
                    // Runtime reattach is not supported yet.
                    result.spoken_override = Some(format!("Run codex remote attach {machine}."));
                }
                _ => {}
            }
        }
        if let Some(widget) = self.voice_widget_for_thread(thread_id) {
            widget.on_voice_command_result(&result);
        }
    }
}
