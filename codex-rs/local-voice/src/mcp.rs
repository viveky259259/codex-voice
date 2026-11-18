//! A small MCP client over streamable HTTP (JSON-RPC over POST), enough to
//! list tools, call them and poll jobs.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::ACCEPT;
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;
use serde_json::json;
use std::sync::Mutex;
use std::sync::PoisonError;

use crate::config::McpJobConfig;
use crate::error::Result;
use crate::error::VoiceError;
use crate::template::substitute_json;

const PROTOCOL_VERSION: &str = "2025-03-26";
const SESSION_HEADER: &str = "mcp-session-id";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";

#[derive(Clone, Debug, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
struct Session {
    id: Option<String>,
    protocol_version: String,
    tools: Vec<McpTool>,
}

pub struct McpClient {
    http: reqwest::Client,
    url: String,
    next_id: AtomicU64,
    session: Mutex<Option<Session>>,
}

impl McpClient {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            http: local_http_client()?,
            url: url.into(),
            next_id: AtomicU64::new(1),
            session: Mutex::new(None),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Initializes (once) and returns the server's tools. Schemas are
    /// logged at debug level.
    pub async fn tools(&self) -> Result<Vec<McpTool>> {
        Ok(self.session().await?.tools)
    }

    /// Fails with a message naming the available tools when one is missing.
    pub async fn require_tools(&self, names: &[(&str, &str)]) -> Result<()> {
        let tools = self.tools().await?;
        for (setting, name) in names {
            if !tools.iter().any(|tool| tool.name == *name) {
                let available: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
                return Err(VoiceError::Mcp(format!(
                    "MCP server {} has no tool `{name}` (set by `{setting}`); available tools: {}",
                    self.url,
                    if available.is_empty() {
                        "none".to_string()
                    } else {
                        available.join(", ")
                    }
                )));
            }
        }
        Ok(())
    }

    /// Calls a tool and returns its payload: `structuredContent` when
    /// present, else the text content parsed as JSON, else
    /// `{"text": ..., "content": [...]}`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(VoiceError::Mcp(format!(
                "tool `{name}` failed: {}",
                if text.is_empty() {
                    result.to_string()
                } else {
                    text
                }
            )));
        }
        if let Some(structured) = result.get("structuredContent")
            && !structured.is_null()
        {
            return Ok(structured.clone());
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(text.trim())
            && (parsed.is_object() || parsed.is_array())
        {
            return Ok(parsed);
        }
        Ok(json!({
            "text": text,
            "content": result.get("content").cloned().unwrap_or(Value::Null),
        }))
    }

    /// Calls `tool` and, when the result names a job, polls the job tool
    /// until it is done. Returns the final payload. `has_output` says
    /// whether a payload already holds the wanted output (a tool that
    /// answers synchronously).
    pub async fn run_job(
        &self,
        tool: &str,
        arguments: Value,
        job: &McpJobConfig,
        has_output: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        let first = self.call_tool(tool, arguments).await?;
        let Some(job_id) = find_job_id(&first, job) else {
            return Ok(first);
        };
        match job_state(&first, job) {
            JobState::Done => return Ok(first),
            JobState::Failed(message) => {
                return Err(VoiceError::Mcp(format!("job {job_id} failed: {message}")));
            }
            JobState::Unknown if has_output(&first) => return Ok(first),
            JobState::Pending | JobState::Unknown => {}
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(job.timeout_ms);
        let arguments = substitute_json(&job.arguments, &[("job_id", job_id.as_str())]);
        loop {
            let polled = self.call_tool(&job.tool, arguments.clone()).await?;
            match job_state(&polled, job) {
                JobState::Done | JobState::Unknown => return Ok(polled),
                JobState::Failed(message) => {
                    return Err(VoiceError::Mcp(format!("job {job_id} failed: {message}")));
                }
                JobState::Pending => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(VoiceError::Mcp(format!(
                    "job {job_id} not done after {} ms",
                    job.timeout_ms
                )));
            }
            tokio::time::sleep(Duration::from_millis(job.poll_interval_ms.max(10))).await;
        }
    }

    async fn session(&self) -> Result<Session> {
        if let Some(session) = self.stored_session() {
            return Ok(session);
        }
        let session = self.initialize().await?;
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Some(session.clone());
        Ok(session)
    }

    fn stored_session(&self) -> Option<Session> {
        self.session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    async fn initialize(&self) -> Result<Session> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "codex-local-voice", "version": env!("CARGO_PKG_VERSION") },
        });
        let (result, session_id) = self.post("initialize", Some(params), None, None).await?;
        let result = result.unwrap_or(Value::Null);
        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(PROTOCOL_VERSION)
            .to_string();
        let mut session = Session {
            id: session_id,
            protocol_version,
            tools: Vec::new(),
        };
        self.post("notifications/initialized", None, Some(&session), None)
            .await?;
        let listed = self.post_request("tools/list", json!({}), &session).await?;
        session.tools = parse_tools(&listed);
        for tool in &session.tools {
            tracing::debug!(
                server = %self.url,
                tool = %tool.name,
                description = tool.description.as_deref().unwrap_or(""),
                schema = %tool.input_schema,
                "MCP tool"
            );
        }
        Ok(session)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let session = self.session().await?;
        match self.post_request(method, params.clone(), &session).await {
            Err(VoiceError::Http(message))
                if message.starts_with("404") && session.id.is_some() =>
            {
                // The server forgot our session: start a new one, once.
                *self.session.lock().unwrap_or_else(PoisonError::into_inner) = None;
                let session = self.session().await?;
                self.post_request(method, params, &session).await
            }
            other => other,
        }
    }

    async fn post_request(&self, method: &str, params: Value, session: &Session) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (result, _) = self
            .post(method, Some(params), Some(session), Some(id))
            .await?;
        result.ok_or_else(|| VoiceError::Mcp(format!("no response to `{method}`")))
    }

    /// Posts one JSON-RPC message. Notifications (no `id`) return `None`.
    async fn post(
        &self,
        method: &str,
        params: Option<Value>,
        session: Option<&Session>,
        id: Option<u64>,
    ) -> Result<(Option<Value>, Option<String>)> {
        let is_initialize = method == "initialize";
        let id = if is_initialize {
            Some(self.next_id.fetch_add(1, Ordering::Relaxed))
        } else {
            id
        };
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        if let Some(id) = id {
            message["id"] = json!(id);
        }
        let mut request = self
            .http
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(message.to_string());
        if let Some(session) = session {
            request = request.header(PROTOCOL_HEADER, &session.protocol_version);
            if let Some(session_id) = &session.id {
                request = request.header(SESSION_HEADER, session_id);
            }
        }
        let response = request.send().await.map_err(|err| {
            VoiceError::Http(format!("cannot reach MCP server {}: {err}", self.url))
        })?;
        let status = response.status();
        let session_id = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(VoiceError::Http(format!(
                "{} from MCP server {} for `{method}`: {}",
                status.as_u16(),
                self.url,
                excerpt(&body)
            )));
        }
        let Some(id) = id else {
            return Ok((None, session_id));
        };
        if status == StatusCode::ACCEPTED || body.trim().is_empty() {
            return Err(VoiceError::Mcp(format!("empty response to `{method}`")));
        }
        let reply = if content_type.starts_with("text/event-stream") {
            find_sse_reply(&body, id)
        } else {
            serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| match value {
                    Value::Array(items) => items
                        .into_iter()
                        .find(|item| item.get("id") == Some(&json!(id))),
                    other => Some(other),
                })
        };
        let reply = reply.ok_or_else(|| {
            VoiceError::Mcp(format!(
                "unreadable response to `{method}`: {}",
                excerpt(&body)
            ))
        })?;
        if let Some(error) = reply.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string());
            return Err(VoiceError::Mcp(format!("`{method}` failed: {message}")));
        }
        Ok((
            Some(reply.get("result").cloned().unwrap_or(Value::Null)),
            session_id,
        ))
    }
}

/// A reqwest client for local servers: never goes through a proxy.
pub(crate) fn local_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(VoiceError::from)
}

fn parse_tools(listed: &Value) -> Vec<McpTool> {
    listed
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    Some(McpTool {
                        name: tool.get("name")?.as_str()?.to_string(),
                        description: tool
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        input_schema: tool.get("inputSchema").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn find_sse_reply(body: &str, id: u64) -> Option<Value> {
    let normalized = body.replace("\r\n", "\n");
    normalized.split("\n\n").find_map(|event| {
        let data: Vec<&str> = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.strip_prefix(' ').unwrap_or(line))
            .collect();
        if data.is_empty() {
            return None;
        }
        let value: Value = serde_json::from_str(&data.join("\n")).ok()?;
        (value.get("id") == Some(&json!(id))).then_some(value)
    })
}

pub(crate) fn excerpt(text: &str) -> String {
    const MAX: usize = 300;
    let text = text.trim();
    match text.char_indices().nth(MAX) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_string(),
    }
}

/// Looks up a dotted path such as `result.text` or `artifacts.0.path`.
pub fn json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in path.split('.').filter(|part| !part.is_empty()) {
        current = match current {
            Value::Object(map) => map.get(part)?,
            Value::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// The first non-empty string found at `configured` or one of `fallbacks`.
pub fn find_string(value: &Value, configured: Option<&str>, fallbacks: &[&str]) -> Option<String> {
    configured
        .into_iter()
        .chain(fallbacks.iter().copied())
        .find_map(|path| match json_path(value, path)? {
            Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
}

const JOB_ID_PATHS: &[&str] = &["job_id", "jobId", "job.id", "job.job_id", "id"];
const STATUS_PATHS: &[&str] = &["status", "state", "job.status", "job.state"];

fn find_job_id(value: &Value, job: &McpJobConfig) -> Option<String> {
    find_string(value, job.job_id_path.as_deref(), JOB_ID_PATHS)
}

enum JobState {
    Pending,
    Done,
    Failed(String),
    Unknown,
}

fn job_state(value: &Value, job: &McpJobConfig) -> JobState {
    let Some(status) = find_string(value, job.status_path.as_deref(), STATUS_PATHS) else {
        return JobState::Unknown;
    };
    let status = status.to_ascii_lowercase();
    if job
        .done_statuses
        .iter()
        .any(|done| done.eq_ignore_ascii_case(&status))
    {
        JobState::Done
    } else if job
        .failed_statuses
        .iter()
        .any(|failed| failed.eq_ignore_ascii_case(&status))
    {
        let message = find_string(value, None, &["error", "message", "job.error", "detail"])
            .unwrap_or(status);
        JobState::Failed(message)
    } else {
        JobState::Pending
    }
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
pub(crate) mod tests;
