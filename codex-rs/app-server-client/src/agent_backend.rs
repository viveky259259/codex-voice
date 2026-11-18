//! Channel-backed client for agent backends that are not Codex app servers.
//!
//! A backend such as `codex-claude-agent-backend` owns the receiving side of the
//! command channel and the sending side of the event channel. It answers each
//! app-server request itself and emits the same notifications and server
//! requests a real app server would, so TUI surfaces work unchanged. Requests
//! it does not support must be answered with an explicit error.

use crate::AppServerEvent;
use crate::RequestResult;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result as JsonRpcResult;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// What the caller asks of the backend. Each operation gets exactly one answer.
pub enum AgentBackendOperation {
    Request(Box<ClientRequest>),
    Notify(ClientNotification),
    /// The answer to a server request the backend emitted.
    Resolve {
        request_id: RequestId,
        result: JsonRpcResult,
    },
    Reject {
        request_id: RequestId,
        error: JSONRPCErrorError,
    },
    Shutdown,
}

/// An operation and its single response.
pub struct AgentBackendCommand {
    pub operation: AgentBackendOperation,
    pub response: oneshot::Sender<io::Result<RequestResult>>,
}

#[derive(Clone)]
pub struct AgentBackendRequestHandle {
    commands: mpsc::Sender<AgentBackendCommand>,
    name: Arc<str>,
}

/// The caller side of an agent backend: requests go out on a bounded channel,
/// events arrive on an unbounded one so unread notifications never block replies.
pub struct AgentBackendClient {
    handle: AgentBackendRequestHandle,
    events: mpsc::UnboundedReceiver<AppServerEvent>,
}

impl AgentBackendClient {
    /// Creates the client and the backend's ends of both channels. `name` is the
    /// backend's display name, such as "Claude Code".
    pub fn channel(
        name: &str,
        capacity: usize,
    ) -> (
        Self,
        mpsc::Receiver<AgentBackendCommand>,
        mpsc::UnboundedSender<AppServerEvent>,
    ) {
        let (commands, receiver) = mpsc::channel(capacity.max(1));
        let (events, event_receiver) = mpsc::unbounded_channel();
        (
            Self {
                handle: AgentBackendRequestHandle {
                    commands,
                    name: Arc::from(name),
                },
                events: event_receiver,
            },
            receiver,
            events,
        )
    }

    pub fn name(&self) -> &str {
        &self.handle.name
    }

    pub fn request_handle(&self) -> AgentBackendRequestHandle {
        self.handle.clone()
    }

    pub async fn next_event(&mut self) -> Option<AppServerEvent> {
        self.events.recv().await
    }

    pub async fn request(&self, request: ClientRequest) -> io::Result<RequestResult> {
        self.handle.request(request).await
    }

    pub async fn notify(&self, notification: ClientNotification) -> io::Result<()> {
        self.handle
            .unit(AgentBackendOperation::Notify(notification))
            .await
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> io::Result<()> {
        self.handle.resolve_server_request(request_id, result).await
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> io::Result<()> {
        self.handle.reject_server_request(request_id, error).await
    }

    pub async fn shutdown(self) -> io::Result<()> {
        tokio::time::timeout(
            crate::SHUTDOWN_TIMEOUT,
            self.handle.unit(AgentBackendOperation::Shutdown),
        )
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} backend shutdown timed out", self.handle.name),
            )
        })?
    }
}

impl AgentBackendRequestHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, operation: AgentBackendOperation) -> io::Result<RequestResult> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(AgentBackendCommand {
                operation,
                response,
            })
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("{} backend is closed", self.name),
                )
            })?;
        result.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("{} backend dropped its response", self.name),
            )
        })?
    }

    async fn unit(&self, operation: AgentBackendOperation) -> io::Result<()> {
        self.send(operation)
            .await?
            .map(|_| ())
            .map_err(|error| io::Error::other(error.message))
    }

    pub async fn request(&self, request: ClientRequest) -> io::Result<RequestResult> {
        self.send(AgentBackendOperation::Request(Box::new(request)))
            .await
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> io::Result<()> {
        self.unit(AgentBackendOperation::Resolve { request_id, result })
            .await
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> io::Result<()> {
        self.unit(AgentBackendOperation::Reject { request_id, error })
            .await
    }
}
