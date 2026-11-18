//! Approval requests from agents on the machines in `[realtime] voice_machines`.
//!
//! Each machine's SSH link (`voice_command_remote`) subscribes to the threads
//! it lists with `thread/resume` and forwards that machine's approval requests
//! here as [`RemoteApprovalEvent`]s. They become ordinary [`ApprovalRequest`]s
//! labeled "devbox · build" in the same approval overlay as this machine's, so
//! keyboard answers and every spoken-approval rule (approve, then "confirm";
//! deny at once) apply unchanged.
//!
//! Routing:
//! - A remote thread id is only unique on its machine, so each (machine,
//!   thread) gets a local alias [`ThreadId`] for the overlay. An answer to an
//!   alias never reaches this machine's app server: [`App::try_answer_remote_approval`]
//!   takes it in `submit_thread_op` and resolves the request on the
//!   connection it came from, as the JSON-RPC response to that server request.
//! - Requests are keyed by (machine, request id), with the connection they
//!   arrived on. `serverRequest/resolved` (answered elsewhere), the end of the
//!   request's turn, or the connection dropping withdraws the prompt.
//! - Nothing is ever answered automatically. A failed reply leaves the request
//!   waiting on its machine and says so.

use super::App;
use super::ThreadInteractiveRequest;
use super::app_server_requests::BufferedServerRequest;
use super::app_server_requests::PendingAppServerRequests;
use super::app_server_requests::ResolvedAppServerRequest;
use super::voice_command_backend::local_machine_name;
use super::voice_command_remote::resolve_voice_machines;
use crate::app_command::AppCommand;
use crate::bottom_pane::ApplyPatchApprovalRequest;
use crate::bottom_pane::ApprovalRequest;
use crate::bottom_pane::ExecApprovalRequest;
use crate::bottom_pane::PermissionsApprovalRequest;
use crate::bottom_pane::remote_thread_label;
use codex_app_server_client::AppServerReplyHandle;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::path::PathBuf;

/// The agent a remote request belongs to, as the machine's link knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteAgent {
    /// The name voice commands use for it ("build").
    pub(crate) name: String,
    /// The thread's working directory on that machine.
    pub(crate) cwd: Option<PathBuf>,
}

/// What a machine's link reports. `connection` numbers the link's
/// connections, so a stale event of a replaced connection is ignored.
#[derive(Debug)]
pub(crate) enum RemoteApprovalEvent {
    /// An approval request from a thread the link subscribed to.
    Request {
        machine: String,
        connection: u64,
        agent: RemoteAgent,
        request: ServerRequest,
        /// Answers on the connection the request arrived on, never another.
        reply: AppServerReplyHandle,
    },
    /// `serverRequest/resolved`: another client answered, or the server
    /// withdrew the request.
    Resolved {
        machine: String,
        thread_id: String,
        request_id: RequestId,
    },
    /// `turn/completed`: the server drops the turn's pending requests without
    /// a resolution notice.
    TurnEnded {
        machine: String,
        thread_id: String,
        turn_id: String,
    },
    /// The connection is gone; its requests can no longer be answered here.
    Disconnected { machine: String, connection: u64 },
}

/// The remote thread and turn of an approval request this module handles.
fn request_target(request: &ServerRequest) -> Option<(&str, &str)> {
    match request {
        ServerRequest::CommandExecutionRequestApproval { params, .. } => {
            Some((params.thread_id.as_str(), params.turn_id.as_str()))
        }
        ServerRequest::FileChangeRequestApproval { params, .. } => {
            Some((params.thread_id.as_str(), params.turn_id.as_str()))
        }
        ServerRequest::PermissionsRequestApproval { params, .. } => {
            Some((params.thread_id.as_str(), params.turn_id.as_str()))
        }
        _ => None,
    }
}

/// The thread id of an approval request this module handles.
pub(crate) fn remote_approval_thread_id(request: &ServerRequest) -> Option<&str> {
    request_target(request).map(|(thread_id, _)| thread_id)
}

/// The prompt for a request from `agent` on `machine`, shown under the local
/// alias `thread_id`. `None` for anything but an exec, patch or permissions
/// approval, and for permissions whose paths this machine cannot represent.
pub(crate) fn remote_approval_request(
    machine: &str,
    agent: &RemoteAgent,
    thread_id: ThreadId,
    request: &ServerRequest,
) -> Option<ApprovalRequest> {
    let thread_label = Some(remote_thread_label(machine, &agent.name));
    match request {
        ServerRequest::CommandExecutionRequestApproval { params, .. } => {
            let available_decisions = params.available_decisions.clone().unwrap_or_else(|| {
                super::default_exec_approval_decisions(
                    params.network_approval_context.as_ref(),
                    params.proposed_execpolicy_amendment.as_ref(),
                    params.proposed_network_policy_amendments.as_deref(),
                    params.additional_permissions.as_ref(),
                )
            });
            Some(ApprovalRequest::Exec(ExecApprovalRequest {
                kind: params.kind,
                thread_id,
                thread_label,
                id: params
                    .approval_id
                    .clone()
                    .unwrap_or_else(|| params.item_id.clone()),
                environment_id: params.environment_id.clone(),
                command: params
                    .command
                    .as_deref()
                    .map(crate::exec_command::split_command_string)
                    .unwrap_or_default(),
                reason: params.reason.clone(),
                available_decisions,
                network_approval_context: params.network_approval_context.clone(),
                additional_permissions: params.additional_permissions.clone(),
            }))
        }
        ServerRequest::FileChangeRequestApproval { params, .. } => {
            // The diff stays on that machine; only its directory labels the prompt.
            let cwd = agent
                .cwd
                .as_ref()
                .and_then(|cwd| AbsolutePathBuf::from_absolute_path(cwd).ok())
                .or_else(|| {
                    AbsolutePathBuf::from_absolute_path(std::path::MAIN_SEPARATOR_STR).ok()
                })?;
            Some(ApprovalRequest::ApplyPatch(ApplyPatchApprovalRequest {
                thread_id,
                thread_label,
                id: params.item_id.clone(),
                reason: params.reason.clone(),
                cwd,
                changes: HashMap::new(),
            }))
        }
        ServerRequest::PermissionsRequestApproval { params, .. } => {
            let permissions = params.permissions.clone().try_into().ok()?;
            Some(ApprovalRequest::Permissions(PermissionsApprovalRequest {
                thread_id,
                thread_label,
                call_id: params.item_id.clone(),
                environment_id: params.environment_id.clone(),
                reason: params.reason.clone(),
                permissions,
            }))
        }
        _ => None,
    }
}

/// The dismissal matching `prompt`.
fn resolution_for(prompt: &ApprovalRequest) -> Option<ResolvedAppServerRequest> {
    match prompt {
        ApprovalRequest::Exec(request) => Some(ResolvedAppServerRequest::ExecApproval {
            thread_id: request.thread_id.to_string(),
            id: request.id.clone(),
        }),
        ApprovalRequest::ApplyPatch(request) => {
            Some(ResolvedAppServerRequest::FileChangeApproval {
                thread_id: request.thread_id.to_string(),
                id: request.id.clone(),
            })
        }
        ApprovalRequest::Permissions(request) => {
            Some(ResolvedAppServerRequest::PermissionsApproval {
                thread_id: request.thread_id.to_string(),
                id: request.call_id.clone(),
            })
        }
        ApprovalRequest::McpElicitation(_) => None,
    }
}

struct RemoteEntry {
    connection: u64,
    /// Arrival order, to show prompts again in order.
    seq: u64,
    remote_thread: String,
    turn_id: String,
    prompt: ApprovalRequest,
    dismissal: ResolvedAppServerRequest,
}

/// An answer ready to go back to the machine that asked.
pub(crate) struct RemoteResponse {
    pub(crate) machine: String,
    pub(crate) request_id: RequestId,
    pub(crate) reply: AppServerReplyHandle,
    pub(crate) result: serde_json::Value,
}

/// Pending approval requests from other machines.
#[derive(Default)]
pub(crate) struct RemoteApprovals {
    /// Keyed by (machine, request id): request ids are per server.
    entries: HashMap<(String, RequestId), RemoteEntry>,
    /// Per machine: the reply handles and the response encoding, keyed by
    /// that machine's thread ids, exactly as for this machine's requests.
    replies: HashMap<String, PendingAppServerRequests>,
    /// (machine, remote thread id) → the alias the overlay uses, and back.
    aliases: HashMap<(String, String), ThreadId>,
    origins: HashMap<ThreadId, (String, String)>,
    next_seq: u64,
}

impl RemoteApprovals {
    fn alias(&mut self, machine: &str, remote_thread: &str) -> ThreadId {
        let key = (machine.to_string(), remote_thread.to_string());
        if let Some(alias) = self.aliases.get(&key) {
            return *alias;
        }
        let alias = ThreadId::new();
        self.aliases.insert(key.clone(), alias);
        self.origins.insert(alias, key);
        alias
    }

    /// The machine an overlay thread id stands for, if it is an alias.
    pub(crate) fn machine_of(&self, thread_id: ThreadId) -> Option<&str> {
        self.origins
            .get(&thread_id)
            .map(|(machine, _)| machine.as_str())
    }

    /// Record a request. Returns the prompt to show, or `None` for a
    /// duplicate (a replay on the same connection) or an unsupported request.
    pub(crate) fn note_request(
        &mut self,
        machine: &str,
        connection: u64,
        agent: &RemoteAgent,
        request: ServerRequest,
        reply: AppServerReplyHandle,
    ) -> Option<ApprovalRequest> {
        let (remote_thread, turn_id) = request_target(&request)
            .map(|(thread, turn)| (thread.to_string(), turn.to_string()))?;
        let key = (machine.to_string(), request.id().clone());
        if let Some(entry) = self.entries.get_mut(&key) {
            // The prompt is already up; answer on the newest connection.
            entry.connection = connection;
            self.replies
                .entry(machine.to_string())
                .or_default()
                .note_buffered_request(&BufferedServerRequest { request, reply });
            return None;
        }
        let alias = self.alias(machine, &remote_thread);
        let prompt = remote_approval_request(machine, agent, alias, &request)?;
        let dismissal = resolution_for(&prompt)?;
        if self
            .replies
            .entry(machine.to_string())
            .or_default()
            .note_buffered_request(&BufferedServerRequest { request, reply })
            .is_some()
        {
            return None;
        }
        self.next_seq += 1;
        self.entries.insert(
            key,
            RemoteEntry {
                connection,
                seq: self.next_seq,
                remote_thread,
                turn_id,
                prompt: prompt.clone(),
                dismissal,
            },
        );
        Some(prompt)
    }

    /// The answer `op` gives to a remote request shown under `thread_id`.
    /// `None` when `thread_id` is not an alias or nothing is pending for `op`.
    pub(crate) fn take_response(
        &mut self,
        thread_id: ThreadId,
        op: &AppCommand,
    ) -> Option<RemoteResponse> {
        let (machine, remote_thread) = self.origins.get(&thread_id)?.clone();
        let resolution = match self
            .replies
            .get_mut(&machine)?
            .take_resolution_record(&remote_thread, op.clone())
        {
            Ok(resolution) => resolution?,
            Err(error) => {
                tracing::warn!("remote approval on {machine}: {error}");
                return None;
            }
        };
        let request_id = resolution.request.request_id;
        self.entries.remove(&(machine.clone(), request_id.clone()));
        Some(RemoteResponse {
            machine,
            request_id,
            reply: resolution.request.reply?,
            result: resolution.result,
        })
    }

    fn remove(
        &mut self,
        machine: &str,
        request_id: &RequestId,
    ) -> Option<ResolvedAppServerRequest> {
        let entry = self
            .entries
            .remove(&(machine.to_string(), request_id.clone()))?;
        if let Some(replies) = self.replies.get_mut(machine) {
            replies.resolve_notification(&entry.remote_thread, request_id);
        }
        Some(entry.dismissal)
    }

    /// `serverRequest/resolved` from `machine`.
    pub(crate) fn resolve(
        &mut self,
        machine: &str,
        thread_id: &str,
        request_id: &RequestId,
    ) -> Option<ResolvedAppServerRequest> {
        let key = (machine.to_string(), request_id.clone());
        if self
            .entries
            .get(&key)
            .is_none_or(|entry| entry.remote_thread != thread_id)
        {
            return None;
        }
        self.remove(machine, request_id)
    }

    /// The requests of a turn that ended on `machine`.
    pub(crate) fn turn_ended(
        &mut self,
        machine: &str,
        thread_id: &str,
        turn_id: &str,
    ) -> Vec<ResolvedAppServerRequest> {
        self.remove_where(machine, |entry| {
            entry.remote_thread == thread_id && entry.turn_id == turn_id
        })
    }

    /// The requests that arrived on a connection that is now gone.
    pub(crate) fn disconnect(
        &mut self,
        machine: &str,
        connection: u64,
    ) -> Vec<ResolvedAppServerRequest> {
        self.remove_where(machine, |entry| entry.connection == connection)
    }

    fn remove_where(
        &mut self,
        machine: &str,
        matches: impl Fn(&RemoteEntry) -> bool,
    ) -> Vec<ResolvedAppServerRequest> {
        let mut ids: Vec<(u64, RequestId)> = self
            .entries
            .iter()
            .filter(|((entry_machine, _), entry)| entry_machine == machine && matches(entry))
            .map(|((_, request_id), entry)| (entry.seq, request_id.clone()))
            .collect();
        ids.sort_by_key(|(seq, _)| *seq);
        ids.into_iter()
            .filter_map(|(_, request_id)| self.remove(machine, &request_id))
            .collect()
    }

    /// Every pending prompt, oldest first, to show again in a new view.
    pub(crate) fn prompts(&self) -> Vec<ApprovalRequest> {
        let mut entries: Vec<&RemoteEntry> = self.entries.values().collect();
        entries.sort_by_key(|entry| entry.seq);
        entries
            .into_iter()
            .map(|entry| entry.prompt.clone())
            .collect()
    }
}

/// Spoken and shown when an answer could not be sent back.
pub(crate) fn unreachable_notice(machine: &str) -> String {
    format!("Couldn't reach {machine}; the approval is still waiting there.")
}

/// Spoken and shown when a connection with open prompts drops.
pub(crate) fn disconnected_notice(machine: &str) -> String {
    format!("Lost {machine}; its approval is still waiting there.")
}

impl App {
    /// Whether `machine` is one of the configured `voice_machines`.
    fn is_voice_machine(&self, machine: &str) -> bool {
        let (machines, _) =
            resolve_voice_machines(&self.config.realtime.voice_machines, &local_machine_name());
        machines.iter().any(|known| known.name == machine)
    }

    pub(super) fn handle_remote_approval_event(&mut self, event: RemoteApprovalEvent) {
        match event {
            RemoteApprovalEvent::Request {
                machine,
                connection,
                agent,
                request,
                reply,
            } => {
                if !self.is_voice_machine(&machine) {
                    tracing::warn!("ignoring an approval request from unlisted machine {machine}");
                    return;
                }
                if let Some(prompt) = self
                    .voice_commands
                    .remote_approvals
                    .note_request(&machine, connection, &agent, request, reply)
                {
                    self.push_thread_interactive_request(ThreadInteractiveRequest::Approval(
                        prompt,
                    ));
                }
            }
            RemoteApprovalEvent::Resolved {
                machine,
                thread_id,
                request_id,
            } => {
                if let Some(dismissal) =
                    self.voice_commands
                        .remote_approvals
                        .resolve(&machine, &thread_id, &request_id)
                {
                    self.chat_widget.dismiss_app_server_request(&dismissal);
                }
            }
            RemoteApprovalEvent::TurnEnded {
                machine,
                thread_id,
                turn_id,
            } => {
                for dismissal in self
                    .voice_commands
                    .remote_approvals
                    .turn_ended(&machine, &thread_id, &turn_id)
                {
                    self.chat_widget.dismiss_app_server_request(&dismissal);
                }
            }
            RemoteApprovalEvent::Disconnected {
                machine,
                connection,
            } => {
                let dismissed = self
                    .voice_commands
                    .remote_approvals
                    .disconnect(&machine, connection);
                if dismissed.is_empty() {
                    return;
                }
                for dismissal in &dismissed {
                    self.chat_widget.dismiss_app_server_request(dismissal);
                }
                self.chat_widget
                    .notify_remote_approval_status(&disconnected_notice(&machine));
            }
        }
    }

    /// Answer a prompt from another machine on the connection it came from.
    /// Returns `true` when `thread_id` belongs to another machine, so `op`
    /// must not reach this machine's app server.
    pub(super) async fn try_answer_remote_approval(
        &mut self,
        thread_id: ThreadId,
        op: &AppCommand,
    ) -> bool {
        let Some(machine) = self
            .voice_commands
            .remote_approvals
            .machine_of(thread_id)
            .map(str::to_string)
        else {
            return false;
        };
        let Some(response) = self
            .voice_commands
            .remote_approvals
            .take_response(thread_id, op)
        else {
            // Already answered or withdrawn, or not an approval answer.
            tracing::debug!("no pending approval on {machine} for this answer");
            return true;
        };
        if let Err(error) = response.reply.resolve(response.result).await {
            tracing::warn!(
                "could not answer approval {:?} on {}: {error}",
                response.request_id,
                response.machine
            );
            self.chat_widget
                .notify_remote_approval_status(&unreachable_notice(&response.machine));
        }
        true
    }

    /// Show the pending prompts from other machines again in a new view.
    pub(super) fn resurface_remote_approvals(&mut self) {
        for prompt in self.voice_commands.remote_approvals.prompts() {
            self.push_thread_interactive_request(ThreadInteractiveRequest::Approval(prompt));
        }
    }
}

#[cfg(test)]
#[path = "voice_remote_approvals_tests.rs"]
mod tests;
