//! Voice orchestration of agents on other machines, over SSH.
//!
//! Each machine listed in `[realtime] voice_machines` gets one
//! [`RemoteLink`]: an [`AppServerTransport`] that reaches that machine's app
//! server through Lane C's `ssh://` endpoint (`ssh HOST -- codex app-server
//! proxy`). Wrapped in `AppServerAgentBackend`, it answers exactly the same
//! requests as this machine's backend.
//!
//! Connection lifecycle:
//! - `start_connecting` (called when voice starts) or the first request
//!   spawns the SSH connect on its own task, so a slow handshake never blocks
//!   the orchestrator, the other machines, or the UI loop.
//! - A request waits for an in-flight connect for at most `connect_wait`
//!   (shorter than the orchestrator's per-machine timeout), then fails with
//!   `BackendError::Connecting` on a first attempt, or `Unreachable` with the
//!   last error on a retry.
//! - Once connected, the connection is kept. A transport failure or a
//!   disconnect drops it; the next request connects again.
//! - Dropping the link aborts its tasks and drops the client, which closes
//!   the WebSocket and kills the ssh child (`kill_on_drop`).
//!
//! Only configured machines ever get a link: discovered machines are names
//! for "list my machines", never SSH targets.

use super::voice_command_backend::AppServerAgentBackend;
use super::voice_command_backend::AppServerTransport;
use super::voice_command_backend::short_host_name;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_client::RequestResult;
use codex_app_server_client::SshTarget;
use codex_app_server_protocol::ClientRequest;
use codex_config::config_toml::VoiceMachine;
use codex_orchestrator::BackendError;
use codex_orchestrator::MachineLink;
use futures::FutureExt;
use futures::future::BoxFuture;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::Weak;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How long a request waits for an in-flight SSH connect before reporting
/// the machine as connecting. Below the orchestrator's 3 s per-machine
/// timeout, so a slow handshake is reported as "connecting", not "down".
pub(crate) const DEFAULT_CONNECT_WAIT: Duration = Duration::from_millis(1500);

/// A configured machine, resolved to the name used in speech and its SSH
/// target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VoiceMachineTarget {
    pub(crate) name: String,
    pub(crate) target: SshTarget,
}

/// The `ssh://` target for a destination `codex remote attach` accepts.
pub(crate) fn ssh_target(host: &str, codex_bin: Option<&str>) -> Result<SshTarget, String> {
    let host = host.trim();
    let url = if host.starts_with("ssh://") {
        host.to_string()
    } else {
        format!("ssh://{host}")
    };
    let mut target = SshTarget::parse_url(&url)?;
    if let Some(codex_bin) = codex_bin.map(str::trim).filter(|bin| !bin.is_empty()) {
        target.remote_codex_bin = Some(codex_bin.to_string());
    }
    Ok(target)
}

/// The host as named in speech: an IP as is, otherwise the first DNS label
/// ("devbox" for `me@devbox.tail1234.ts.net:2222`).
fn spoken_host(host: &str) -> String {
    if host.parse::<IpAddr>().is_ok() {
        host.to_string()
    } else {
        short_host_name(host)
    }
}

/// Resolve `[realtime] voice_machines`. Returns the machines to connect to and
/// one warning per entry that was skipped (invalid, duplicate, or this
/// machine).
pub(crate) fn resolve_voice_machines(
    machines: &[VoiceMachine],
    local_machine: &str,
) -> (Vec<VoiceMachineTarget>, Vec<String>) {
    let mut resolved: Vec<VoiceMachineTarget> = Vec::new();
    let mut warnings = Vec::new();
    for machine in machines {
        let target = match ssh_target(&machine.host, machine.codex_bin.as_deref()) {
            Ok(target) => target,
            Err(error) => {
                warnings.push(format!(
                    "voice_machines: skipping `{}`: {error}",
                    machine.host
                ));
                continue;
            }
        };
        if resolved
            .iter()
            .any(|known| known.target.display_url() == target.display_url())
        {
            warnings.push(format!(
                "voice_machines: `{}` is listed twice",
                machine.host
            ));
            continue;
        }
        let taken =
            |name: &str| name == local_machine || resolved.iter().any(|known| known.name == name);
        let configured = machine
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let name = match configured {
            Some(name) => name.to_string(),
            None => {
                let short = spoken_host(&target.host);
                if taken(&short) {
                    target.host.clone()
                } else {
                    short
                }
            }
        };
        if taken(&name) {
            warnings.push(format!(
                "voice_machines: skipping `{}`: the name `{name}` is already used{}",
                machine.host,
                if name == local_machine {
                    " by this machine"
                } else {
                    ""
                }
            ));
            continue;
        }
        resolved.push(VoiceMachineTarget { name, target });
    }
    (resolved, warnings)
}

/// One backend per configured machine, each already connecting in the
/// background.
pub(crate) fn remote_backends(
    machines: Vec<VoiceMachineTarget>,
) -> Vec<AppServerAgentBackend<RemoteLink>> {
    machines
        .into_iter()
        .map(|machine| {
            let link = RemoteLink::new(machine.target);
            link.start_connecting();
            AppServerAgentBackend::new(machine.name, link)
        })
        .collect()
}

/// Aborts the task when dropped, so replacing a link state stops its work.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum LinkState {
    /// Not connected. `last_error` is `None` before the first attempt.
    Down { last_error: Option<String> },
    /// An SSH connect is running on `_task`.
    Connecting {
        attempt: u64,
        previous_error: Option<String>,
        _task: AbortOnDrop,
    },
    /// Connected. `_events` owns the client and drains its events.
    Up {
        attempt: u64,
        handle: AppServerRequestHandle,
        _events: AbortOnDrop,
    },
}

struct Inner {
    state: LinkState,
    attempts: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    /// Bumped on every state change, to wake waiting requests.
    changed: watch::Sender<u64>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self) {
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    fn finish_connect(
        self: &Arc<Self>,
        attempt: u64,
        result: std::io::Result<RemoteAppServerClient>,
    ) {
        let mut inner = self.lock();
        if !matches!(inner.state, LinkState::Connecting { attempt: current, .. } if current == attempt)
        {
            // Superseded; dropping a client here closes its connection.
            return;
        }
        inner.state = match result {
            Ok(client) => {
                let handle = AppServerRequestHandle::Remote(client.request_handle());
                let events = tokio::spawn(watch_events(client, Arc::downgrade(self), attempt));
                LinkState::Up {
                    attempt,
                    handle,
                    _events: AbortOnDrop(events),
                }
            }
            Err(error) => LinkState::Down {
                last_error: Some(first_line(&error.to_string())),
            },
        };
        drop(inner);
        self.notify();
    }

    /// Drop the connection made by `attempt`, if it is still the current one.
    fn mark_down(&self, attempt: u64, error: String) {
        let mut inner = self.lock();
        let current = match &inner.state {
            LinkState::Up { attempt, .. } | LinkState::Connecting { attempt, .. } => *attempt,
            LinkState::Down { .. } => return,
        };
        if current != attempt {
            return;
        }
        inner.state = LinkState::Down {
            last_error: Some(first_line(&error)),
        };
        drop(inner);
        self.notify();
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("connection failed")
        .to_string()
}

/// Read and discard the connection's notifications (the orchestrator only
/// makes requests), and mark the link down when the connection ends. Server
/// requests such as approvals are left for the machine's own clients.
async fn watch_events(mut client: RemoteAppServerClient, shared: Weak<Shared>, attempt: u64) {
    let reason = loop {
        match client.next_event().await {
            Some(AppServerEvent::Disconnected { message }) => break message,
            Some(_) => {}
            None => break "connection closed".to_string(),
        }
    };
    if let Some(shared) = shared.upgrade() {
        shared.mark_down(attempt, reason);
    }
}

fn connect_args(target: SshTarget) -> RemoteAppServerConnectArgs {
    RemoteAppServerConnectArgs {
        // This connection never answers approvals; the machine's own clients
        // keep doing that.
        required_human_approval: false,
        endpoint: RemoteAppServerEndpoint::Ssh { target },
        client_name: "codex-tui-voice-orchestrator".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
    }
}

/// A lazily connected, self-healing app-server connection to one machine
/// over SSH.
pub(crate) struct RemoteLink {
    target: SshTarget,
    connect_wait: Duration,
    shared: Arc<Shared>,
}

impl RemoteLink {
    pub(crate) fn new(target: SshTarget) -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            target,
            connect_wait: DEFAULT_CONNECT_WAIT,
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    state: LinkState::Down { last_error: None },
                    attempts: 0,
                }),
                changed,
            }),
        }
    }

    /// How long a request waits for an in-flight connect.
    pub(crate) fn with_connect_wait(mut self, connect_wait: Duration) -> Self {
        self.connect_wait = connect_wait;
        self
    }

    /// Start connecting in the background unless connected or connecting.
    pub(crate) fn start_connecting(&self) {
        let mut inner = self.shared.lock();
        self.spawn_connect(&mut inner);
    }

    fn spawn_connect(&self, inner: &mut Inner) {
        let LinkState::Down { last_error } = &inner.state else {
            return;
        };
        let previous_error = last_error.clone();
        inner.attempts += 1;
        let attempt = inner.attempts;
        let shared = Arc::downgrade(&self.shared);
        let args = connect_args(self.target.clone());
        let task = tokio::spawn(async move {
            let result = RemoteAppServerClient::connect(args).await;
            if let Some(shared) = shared.upgrade() {
                shared.finish_connect(attempt, result);
            }
        });
        inner.state = LinkState::Connecting {
            attempt,
            previous_error,
            _task: AbortOnDrop(task),
        };
        self.shared.notify();
    }

    /// The connected handle, connecting first if needed.
    async fn ready(&self) -> Result<(u64, AppServerRequestHandle), BackendError> {
        let mut changes = self.shared.changed.subscribe();
        let deadline = tokio::time::Instant::now() + self.connect_wait;
        let mut waited = false;
        loop {
            {
                let mut inner = self.shared.lock();
                match &inner.state {
                    LinkState::Up {
                        attempt, handle, ..
                    } => return Ok((*attempt, handle.clone())),
                    LinkState::Down { last_error } if waited => {
                        return Err(BackendError::Unreachable(
                            last_error
                                .clone()
                                .unwrap_or_else(|| "not connected".to_string()),
                        ));
                    }
                    LinkState::Down { .. } => self.spawn_connect(&mut inner),
                    LinkState::Connecting { .. } => {}
                }
            }
            waited = true;
            if tokio::time::timeout_at(deadline, changes.changed())
                .await
                .is_err()
            {
                return Err(match &self.shared.lock().state {
                    LinkState::Connecting {
                        previous_error: Some(error),
                        ..
                    }
                    | LinkState::Down {
                        last_error: Some(error),
                    } => BackendError::Unreachable(error.clone()),
                    LinkState::Connecting { .. }
                    | LinkState::Down { last_error: None }
                    | LinkState::Up { .. } => BackendError::Connecting,
                });
            }
        }
    }
}

impl AppServerTransport for RemoteLink {
    fn send(&self, request: ClientRequest) -> BoxFuture<'_, Result<RequestResult, BackendError>> {
        async move {
            let (attempt, handle) = self.ready().await?;
            let method = request.method_name();
            match handle.request(request).await {
                Ok(result) => Ok(result),
                Err(error) => {
                    // The connection is gone; the next request reconnects.
                    let message = format!("{method}: {error}");
                    self.shared.mark_down(attempt, message.clone());
                    Err(BackendError::Unreachable(message))
                }
            }
        }
        .boxed()
    }

    fn link(&self) -> MachineLink {
        match &self.shared.lock().state {
            LinkState::Up { .. } => MachineLink::Connected,
            LinkState::Connecting {
                previous_error: None,
                ..
            } => MachineLink::Connecting,
            LinkState::Down { last_error: None } => MachineLink::Disconnected,
            LinkState::Connecting {
                previous_error: Some(_),
                ..
            }
            | LinkState::Down {
                last_error: Some(_),
            } => MachineLink::Unreachable,
        }
    }
}

#[cfg(test)]
#[path = "voice_command_remote_tests.rs"]
pub(crate) mod tests;
