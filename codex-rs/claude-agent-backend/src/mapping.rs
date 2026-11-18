//! Maps one turn of the CLI's stream-json output onto app-server notifications.
//!
//! Pure: the caller supplies the clock and the file reader, so every mapping can
//! be unit tested without a process.

use crate::diff;
use codex_app_server_protocol::AgentMessageDeltaNotification;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallStatus;
use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::McpToolCallError;
use codex_app_server_protocol::McpToolCallResult;
use codex_app_server_protocol::McpToolCallStatus;
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::ReasoningSummaryTextDeltaNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::TokenUsageBreakdown;
use codex_app_server_protocol::TurnError;
use codex_utils_path_uri::LegacyAppPathString;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

/// How a Claude Code tool is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolKind {
    /// `Bash` → `CommandExecution`.
    Command,
    /// `Edit`, `Write`, `MultiEdit`, `NotebookEdit` → `FileChange`.
    FileChange,
    /// `mcp__<server>__<tool>` → `McpToolCall`.
    Mcp { server: String, tool: String },
    /// Everything else → `DynamicToolCall`.
    Dynamic,
}

pub fn tool_kind(name: &str) -> ToolKind {
    if name == "Bash" {
        return ToolKind::Command;
    }
    if diff::is_edit_tool(name) {
        return ToolKind::FileChange;
    }
    if let Some(rest) = name.strip_prefix("mcp__")
        && let Some((server, tool)) = rest.split_once("__")
    {
        return ToolKind::Mcp {
            server: server.to_string(),
            tool: tool.to_string(),
        };
    }
    ToolKind::Dynamic
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BlockKind {
    Text,
    Thinking,
}

struct OpenBlock {
    item_id: String,
    kind: BlockKind,
    text: String,
    started_ms: i64,
}

struct Tool {
    name: String,
    kind: ToolKind,
    input: Value,
    started_ms: i64,
    changes: Vec<FileUpdateChange>,
    done: bool,
}

pub struct TurnMapper {
    thread_id: String,
    turn_id: String,
    cwd: PathBuf,
    message_id: Option<String>,
    open_blocks: HashMap<u64, OpenBlock>,
    stream_counts: HashMap<(String, BlockKind), usize>,
    snapshot_counts: HashMap<(String, BlockKind), usize>,
    finished: HashSet<String>,
    tools: HashMap<String, Tool>,
    tool_order: Vec<String>,
    declined: HashSet<String>,
}

impl TurnMapper {
    pub fn new(thread_id: &str, turn_id: &str, cwd: &Path) -> Self {
        Self {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            cwd: cwd.to_path_buf(),
            message_id: None,
            open_blocks: HashMap::new(),
            stream_counts: HashMap::new(),
            snapshot_counts: HashMap::new(),
            finished: HashSet::new(),
            tools: HashMap::new(),
            tool_order: Vec::new(),
            declined: HashSet::new(),
        }
    }

    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// The user (or a rule) denied this tool; its result is shown as declined.
    pub fn mark_declined(&mut self, tool_use_id: &str) {
        self.declined.insert(tool_use_id.to_string());
    }

    /// One stdout message from the CLI. Subagent output (`parent_tool_use_id`
    /// set) is not shown.
    pub fn on_message(
        &mut self,
        message: &Value,
        now_ms: i64,
        read: &dyn Fn(&Path) -> Option<String>,
    ) -> Vec<ServerNotification> {
        if message
            .get("parent_tool_use_id")
            .is_some_and(|parent| !parent.is_null())
        {
            return Vec::new();
        }
        match message.get("type").and_then(Value::as_str) {
            Some("stream_event") => message
                .get("event")
                .map(|event| self.on_stream_event(event, now_ms))
                .unwrap_or_default(),
            Some("assistant") => self.on_assistant(message, now_ms, read),
            Some("user") => self.on_user(message, now_ms),
            _ => Vec::new(),
        }
    }

    fn on_stream_event(&mut self, event: &Value, now_ms: i64) -> Vec<ServerNotification> {
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.message_id = event
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Vec::new()
            }
            Some("content_block_start") => {
                let block = event.get("content_block").unwrap_or(&Value::Null);
                let (kind, initial) = match block.get("type").and_then(Value::as_str) {
                    Some("text") => (BlockKind::Text, block.get("text")),
                    Some("thinking") => (BlockKind::Thinking, block.get("thinking")),
                    _ => return Vec::new(),
                };
                let message_id = self.message_id.clone().unwrap_or_else(|| "message".into());
                let count = self
                    .stream_counts
                    .entry((message_id.clone(), kind))
                    .or_default();
                let item_id = block_item_id(&message_id, kind, *count);
                *count += 1;
                let mut out = vec![self.started(empty_block(&item_id, kind), now_ms)];
                self.open_blocks.insert(
                    index,
                    OpenBlock {
                        item_id,
                        kind,
                        text: String::new(),
                        started_ms: now_ms,
                    },
                );
                if let Some(text) = initial.and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    out.extend(self.delta(index, text));
                }
                out
            }
            Some("content_block_delta") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                let text = match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => delta.get("text"),
                    Some("thinking_delta") => delta.get("thinking"),
                    _ => None,
                };
                text.and_then(Value::as_str)
                    .map(|text| self.delta(index, text))
                    .unwrap_or_default()
            }
            Some("content_block_stop") => self
                .open_blocks
                .remove(&index)
                .map(|block| vec![self.complete_block(block, now_ms)])
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn delta(&mut self, index: u64, text: &str) -> Vec<ServerNotification> {
        let Some(block) = self.open_blocks.get_mut(&index) else {
            return Vec::new();
        };
        block.text.push_str(text);
        let (thread_id, turn_id, item_id) = (
            self.thread_id.clone(),
            self.turn_id.clone(),
            block.item_id.clone(),
        );
        vec![match block.kind {
            BlockKind::Text => {
                ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
                    thread_id,
                    turn_id,
                    item_id,
                    delta: text.to_string(),
                })
            }
            BlockKind::Thinking => ServerNotification::ReasoningSummaryTextDelta(
                ReasoningSummaryTextDeltaNotification {
                    thread_id,
                    turn_id,
                    item_id,
                    delta: text.to_string(),
                    summary_index: 0,
                },
            ),
        }]
    }

    fn complete_block(&mut self, block: OpenBlock, now_ms: i64) -> ServerNotification {
        self.finished.insert(block.item_id.clone());
        self.completed(
            full_block(&block.item_id, block.kind, block.text),
            Some(block.started_ms),
            now_ms,
        )
    }

    fn on_assistant(
        &mut self,
        message: &Value,
        now_ms: i64,
        read: &dyn Fn(&Path) -> Option<String>,
    ) -> Vec<ServerNotification> {
        let message_id = message
            .pointer("/message/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.message_id.clone())
            .unwrap_or_else(|| "message".into());
        let blocks = message
            .pointer("/message/content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for block in blocks {
            let (kind, text) = match block.get("type").and_then(Value::as_str) {
                Some("text") => (BlockKind::Text, block.get("text")),
                Some("thinking") => (BlockKind::Thinking, block.get("thinking")),
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    out.extend(self.start_tool(id, name, input, now_ms, read));
                    continue;
                }
                _ => continue,
            };
            let count = self
                .snapshot_counts
                .entry((message_id.clone(), kind))
                .or_default();
            let item_id = block_item_id(&message_id, kind, *count);
            *count += 1;
            let text = text.and_then(Value::as_str).unwrap_or_default().to_string();
            let streaming = self
                .open_blocks
                .values()
                .any(|open| open.item_id == item_id);
            if self.finished.contains(&item_id) || streaming || text.is_empty() {
                continue;
            }
            // The stream did not deliver this block: show the snapshot whole.
            self.finished.insert(item_id.clone());
            out.push(self.started(empty_block(&item_id, kind), now_ms));
            out.push(self.completed(full_block(&item_id, kind, text), Some(now_ms), now_ms));
        }
        out
    }

    /// Starts the tool's item once; later calls (a permission request after the
    /// assistant snapshot) return nothing.
    pub fn start_tool(
        &mut self,
        id: &str,
        name: &str,
        input: Value,
        now_ms: i64,
        read: &dyn Fn(&Path) -> Option<String>,
    ) -> Vec<ServerNotification> {
        if id.is_empty() || self.tools.contains_key(id) {
            return Vec::new();
        }
        let kind = tool_kind(name);
        let changes = if kind == ToolKind::FileChange {
            diff::file_changes(name, &input, &self.cwd, read)
        } else {
            Vec::new()
        };
        let tool = Tool {
            name: name.to_string(),
            kind,
            input,
            started_ms: now_ms,
            changes,
            done: false,
        };
        let payload = self.tool_payload(id, &tool, ToolOutcome::Running);
        self.tools.insert(id.to_string(), tool);
        self.tool_order.push(id.to_string());
        vec![self.started(payload, now_ms)]
    }

    fn on_user(&mut self, message: &Value, now_ms: i64) -> Vec<ServerNotification> {
        let blocks = message
            .pointer("/message/content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
            let content = block.get("content").cloned().unwrap_or(Value::Null);
            let outcome = if self.declined.contains(id) {
                ToolOutcome::Declined(content)
            } else if is_error {
                ToolOutcome::Failed(content)
            } else {
                ToolOutcome::Succeeded(content)
            };
            out.extend(self.finish_tool(id, outcome, now_ms));
        }
        out
    }

    fn finish_tool(
        &mut self,
        id: &str,
        outcome: ToolOutcome,
        now_ms: i64,
    ) -> Option<ServerNotification> {
        let tool = self.tools.get(id).filter(|tool| !tool.done)?;
        let payload = self.tool_payload(id, tool, outcome);
        let started = tool.started_ms;
        if let Some(tool) = self.tools.get_mut(id) {
            tool.done = true;
        }
        Some(self.completed(payload, Some(started), now_ms))
    }

    /// Closes everything still open when the turn ends.
    pub fn finish(&mut self, now_ms: i64) -> Vec<ServerNotification> {
        let mut indices: Vec<u64> = self.open_blocks.keys().copied().collect();
        indices.sort_unstable();
        let mut out = Vec::new();
        for index in indices {
            if let Some(block) = self.open_blocks.remove(&index) {
                out.push(self.complete_block(block, now_ms));
            }
        }
        for id in self.tool_order.clone() {
            let outcome = if self.declined.contains(&id) {
                ToolOutcome::Declined(Value::Null)
            } else {
                ToolOutcome::Failed(Value::String(
                    "The turn ended before this tool finished.".into(),
                ))
            };
            out.extend(self.finish_tool(&id, outcome, now_ms));
        }
        out
    }

    fn tool_payload(&self, id: &str, tool: &Tool, outcome: ToolOutcome) -> ThreadItemPayload {
        let duration = match outcome {
            ToolOutcome::Running => None,
            _ => Some(0),
        };
        let text = outcome.content().map(result_text);
        match &tool.kind {
            ToolKind::Command => ThreadItemPayload::CommandExecution {
                sandbox_type: None,
                model_context: None,
                id: id.to_string(),
                plugin_id: None,
                script_path: None,
                command: tool
                    .input
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                cwd: LegacyAppPathString::from_path(&self.cwd),
                process_id: None,
                source: CommandExecutionSource::Agent,
                status: match outcome {
                    ToolOutcome::Running => CommandExecutionStatus::InProgress,
                    ToolOutcome::Succeeded(_) => CommandExecutionStatus::Completed,
                    ToolOutcome::Failed(_) => CommandExecutionStatus::Failed,
                    ToolOutcome::Declined(_) => CommandExecutionStatus::Declined,
                },
                command_actions: Vec::new(),
                aggregated_output: text,
                // Claude Code reports failure, not the exit status.
                exit_code: match outcome {
                    ToolOutcome::Succeeded(_) => Some(0),
                    ToolOutcome::Failed(_) => Some(1),
                    ToolOutcome::Running | ToolOutcome::Declined(_) => None,
                },
                duration_ms: duration,
            },
            ToolKind::FileChange => ThreadItemPayload::FileChange {
                id: id.to_string(),
                changes: tool.changes.clone(),
                status: match outcome {
                    ToolOutcome::Running => PatchApplyStatus::InProgress,
                    ToolOutcome::Succeeded(_) => PatchApplyStatus::Completed,
                    ToolOutcome::Failed(_) => PatchApplyStatus::Failed,
                    ToolOutcome::Declined(_) => PatchApplyStatus::Declined,
                },
            },
            ToolKind::Mcp { server, tool: name } => {
                let (status, result, error) = match &outcome {
                    ToolOutcome::Running => (McpToolCallStatus::InProgress, None, None),
                    ToolOutcome::Succeeded(content) => (
                        McpToolCallStatus::Completed,
                        Some(Box::new(McpToolCallResult {
                            content: content_blocks(content),
                            structured_content: None,
                            meta: None,
                        })),
                        None,
                    ),
                    ToolOutcome::Failed(content) | ToolOutcome::Declined(content) => (
                        McpToolCallStatus::Failed,
                        None,
                        Some(McpToolCallError {
                            message: result_text(content),
                        }),
                    ),
                };
                ThreadItemPayload::McpToolCall {
                    id: id.to_string(),
                    server: server.clone(),
                    environment_id: None,
                    tool: name.clone(),
                    status,
                    arguments: tool.input.clone(),
                    app_context: None,
                    mcp_app_resource_uri: None,
                    mcp_app_ui: None,
                    plugin_id: None,
                    read_only_hint: None,
                    result,
                    error,
                    duration_ms: duration,
                }
            }
            ToolKind::Dynamic => ThreadItemPayload::DynamicToolCall {
                id: id.to_string(),
                namespace: None,
                tool: tool.name.clone(),
                arguments: tool.input.clone(),
                status: match outcome {
                    ToolOutcome::Running => DynamicToolCallStatus::InProgress,
                    ToolOutcome::Succeeded(_) => DynamicToolCallStatus::Completed,
                    ToolOutcome::Failed(_) | ToolOutcome::Declined(_) => {
                        DynamicToolCallStatus::Failed
                    }
                },
                content_items: text
                    .map(|text| vec![DynamicToolCallOutputContentItem::InputText { text }]),
                success: match outcome {
                    ToolOutcome::Running => None,
                    ToolOutcome::Succeeded(_) => Some(true),
                    ToolOutcome::Failed(_) | ToolOutcome::Declined(_) => Some(false),
                },
                duration_ms: duration,
            },
        }
    }

    fn started(&self, payload: ThreadItemPayload, now_ms: i64) -> ServerNotification {
        ServerNotification::ItemStarted(ItemStartedNotification::new(
            self.thread_id.clone(),
            self.turn_id.clone(),
            payload,
            now_ms,
        ))
    }

    fn completed(
        &self,
        payload: ThreadItemPayload,
        started_ms: Option<i64>,
        now_ms: i64,
    ) -> ServerNotification {
        ServerNotification::ItemCompleted(ItemCompletedNotification::new(
            self.thread_id.clone(),
            self.turn_id.clone(),
            payload,
            started_ms,
            now_ms,
        ))
    }
}

enum ToolOutcome {
    Running,
    Succeeded(Value),
    Failed(Value),
    Declined(Value),
}

impl ToolOutcome {
    fn content(&self) -> Option<&Value> {
        match self {
            Self::Running => None,
            Self::Succeeded(content) | Self::Failed(content) | Self::Declined(content) => {
                Some(content)
            }
        }
    }
}

fn block_item_id(message_id: &str, kind: BlockKind, index: usize) -> String {
    let kind = match kind {
        BlockKind::Text => "text",
        BlockKind::Thinking => "thinking",
    };
    format!("{message_id}:{kind}:{index}")
}

fn empty_block(item_id: &str, kind: BlockKind) -> ThreadItemPayload {
    full_block(item_id, kind, String::new())
}

fn full_block(item_id: &str, kind: BlockKind, text: String) -> ThreadItemPayload {
    match kind {
        BlockKind::Text => ThreadItemPayload::AgentMessage {
            moderation: None,
            id: item_id.to_string(),
            text,
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        },
        BlockKind::Thinking => ThreadItemPayload::Reasoning {
            id: item_id.to_string(),
            summary: if text.is_empty() {
                Vec::new()
            } else {
                vec![text]
            },
            content: Vec::new(),
        },
    }
}

/// A tool result's text: a string, or the text blocks of a content array.
pub fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn content_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::Array(blocks) => blocks.clone(),
        Value::Null => Vec::new(),
        other => vec![serde_json::json!({ "type": "text", "text": result_text(other) })],
    }
}

/// `result.usage` as Codex counts it: input includes cached and cache-written tokens.
pub fn usage_breakdown(usage: &Value) -> TokenUsageBreakdown {
    let count = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    let cached = count("cache_read_input_tokens");
    let written = count("cache_creation_input_tokens");
    let input = count("input_tokens") + cached + written;
    let output = count("output_tokens");
    TokenUsageBreakdown {
        total_tokens: input + output,
        input_tokens: input,
        cached_input_tokens: cached,
        cache_write_input_tokens: written,
        output_tokens: output,
        reasoning_output_tokens: 0,
    }
}

/// The turn error a `result` message reports, if the run failed.
pub fn result_error(result: &Value) -> Option<TurnError> {
    let subtype = result
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("success");
    let is_error = result.get("is_error").and_then(Value::as_bool) == Some(true);
    if subtype == "success" && !is_error {
        return None;
    }
    let errors: Vec<&str> = result
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let message = if !errors.is_empty() {
        errors.join("\n")
    } else if let Some(text) = result
        .get("result")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        text.to_string()
    } else {
        format!("Claude Code ended the turn with {subtype}.")
    };
    Some(TurnError {
        message,
        codex_error_info: None,
        additional_details: None,
        misalignment: None,
    })
}

#[cfg(test)]
#[path = "mapping_tests.rs"]
mod tests;
