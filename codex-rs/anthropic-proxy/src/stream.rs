//! Translates an Anthropic Messages event stream into Responses API events.

use std::collections::BTreeMap;
use std::collections::HashMap;

use bytes::Bytes;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use tracing::debug;

use crate::reasoning::encode_redacted_thinking;
use crate::reasoning::encode_server_blocks;
use crate::reasoning::encode_thinking;
use crate::reasoning::is_server_block_type;
use crate::tools::CUSTOM_TOOL_INPUT_PROPERTY;
use crate::tools::ToolKind;
use crate::tools::ToolRegistry;
use crate::tools::ToolTarget;

/// Encodes one Responses event as an SSE frame.
pub(crate) fn encode_sse(event: &Value) -> Bytes {
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    Bytes::from(format!("event: {kind}\ndata: {event}\n\n"))
}

/// Maps an Anthropic error type to the Responses error code Codex keys on.
pub(crate) fn responses_error_code(anthropic_type: &str, message: &str) -> &'static str {
    match anthropic_type {
        "overloaded_error" => "server_is_overloaded",
        "rate_limit_error" => "rate_limit_exceeded",
        "invalid_request_error" | "request_too_large" if is_context_overflow(message) => {
            "context_length_exceeded"
        }
        "invalid_request_error" | "request_too_large" => "invalid_prompt",
        "authentication_error" => "invalid_api_key",
        "permission_error" => "permission_denied",
        "not_found_error" => "model_not_found",
        _ => "server_error",
    }
}

pub(crate) fn is_context_overflow(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("prompt is too long")
        || message.contains("context window")
        || message.contains("context length")
        || message.contains("too many tokens")
}

#[derive(Debug, Default, Clone, Copy)]
struct Usage {
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
}

impl Usage {
    fn update(&mut self, usage: &Value) {
        let read = |key: &str| usage.get(key).and_then(Value::as_u64);
        if let Some(value) = read("input_tokens") {
            self.input_tokens = value;
        }
        if let Some(value) = read("cache_creation_input_tokens") {
            self.cache_creation_input_tokens = value;
        }
        if let Some(value) = read("cache_read_input_tokens") {
            self.cache_read_input_tokens = value;
        }
        if let Some(value) = read("output_tokens") {
            self.output_tokens = value;
        }
    }

    /// Responses usage counts cached and cache-write tokens inside
    /// `input_tokens`; Anthropic reports them separately.
    fn to_responses(self) -> Value {
        let input_tokens =
            self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens;
        json!({
            "input_tokens": input_tokens,
            "input_tokens_details": {
                "cached_tokens": self.cache_read_input_tokens,
                "cache_write_tokens": self.cache_creation_input_tokens,
            },
            "output_tokens": self.output_tokens,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": input_tokens + self.output_tokens,
        })
    }
}

#[derive(Debug)]
enum BlockState {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
        part_added: bool,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        call_id: String,
        target: ToolTarget,
        arguments: String,
        initial_input: Option<Value>,
    },
}

#[derive(Debug)]
struct OpenBlock {
    output_index: usize,
    item_id: String,
    state: BlockState,
}

/// A server tool block (`server_tool_use` or a `*_tool_result`) being
/// streamed. Server tools run inside Anthropic's API, so these blocks are
/// replayed verbatim, never executed by Codex.
#[derive(Debug)]
struct ServerBlock {
    block: Value,
    input_json: String,
}

/// A web search shown to Codex as a `web_search_call` item.
#[derive(Debug)]
struct WebSearch {
    item_id: String,
    output_index: usize,
    query: String,
}

/// Stateful translator for one Anthropic response stream.
#[derive(Debug)]
pub(crate) struct StreamTranslator {
    tools: ToolRegistry,
    model: String,
    id_stem: String,
    created: bool,
    finished: bool,
    next_output_index: usize,
    blocks: BTreeMap<u64, OpenBlock>,
    server_blocks: BTreeMap<u64, ServerBlock>,
    /// Finished server tool blocks not yet stored in a reasoning item, with
    /// the index of the first one.
    server_run: Vec<Value>,
    server_run_start: u64,
    /// Open `web_search_call` items by `server_tool_use` id.
    searches: HashMap<String, WebSearch>,
    output: Vec<Value>,
    usage: Usage,
    stop_reason: Option<String>,
    stop_details: Option<Value>,
}

impl StreamTranslator {
    pub(crate) fn new(tools: ToolRegistry, model: String) -> Self {
        Self {
            tools,
            model,
            id_stem: uuid::Uuid::new_v4().simple().to_string(),
            created: false,
            finished: false,
            next_output_index: 0,
            blocks: BTreeMap::new(),
            server_blocks: BTreeMap::new(),
            server_run: Vec::new(),
            server_run_start: 0,
            searches: HashMap::new(),
            output: Vec::new(),
            usage: Usage::default(),
            stop_reason: None,
            stop_details: None,
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    fn response_id(&self) -> String {
        format!("resp_{}", self.id_stem)
    }

    /// Handles one decoded Anthropic SSE event (`data:` payload).
    pub(crate) fn on_event(&mut self, event: &Value) -> Vec<Value> {
        if self.finished {
            return Vec::new();
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "message_start" => {
                let message = event.get("message");
                if let Some(id) = message
                    .and_then(|message| message.get("id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    self.id_stem = id.strip_prefix("msg_").unwrap_or(id).to_string();
                }
                if let Some(model) = message
                    .and_then(|message| message.get("model"))
                    .and_then(Value::as_str)
                {
                    self.model = model.to_string();
                }
                if let Some(usage) = message.and_then(|message| message.get("usage")) {
                    self.usage.update(usage);
                }
                self.ensure_created()
            }
            "content_block_start" => {
                let mut events = self.ensure_created();
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                events.extend(self.start_block(index, &block));
                events
            }
            "content_block_delta" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                self.apply_delta(index, &delta)
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                self.stop_block(index)
            }
            "message_delta" => {
                let delta = event.get("delta");
                if let Some(stop_reason) = delta
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(stop_reason.to_string());
                }
                if let Some(stop_details) = delta
                    .and_then(|delta| delta.get("stop_details"))
                    .or_else(|| event.get("stop_details"))
                    .filter(|details| details.is_object())
                {
                    self.stop_details = Some(stop_details.clone());
                }
                if let Some(usage) = event.get("usage") {
                    self.usage.update(usage);
                }
                Vec::new()
            }
            "message_stop" => self.complete(),
            "error" => {
                let error = event.get("error");
                let error_type = error
                    .and_then(|error| error.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("api_error");
                let message = error
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Anthropic stream error");
                self.fail(
                    responses_error_code(error_type, message),
                    &format!("Anthropic API error ({error_type}): {message}"),
                )
            }
            "ping" => Vec::new(),
            other => {
                debug!(event_type = other, "ignoring Anthropic stream event");
                Vec::new()
            }
        }
    }

    /// Called when the upstream stream ends; emits a failure if Anthropic
    /// never sent `message_stop`.
    pub(crate) fn finish(&mut self) -> Vec<Value> {
        if self.finished {
            return Vec::new();
        }
        self.fail("server_error", "Anthropic stream ended before message_stop")
    }

    /// Terminates the response with `response.failed`.
    pub(crate) fn fail(&mut self, code: &str, message: &str) -> Vec<Value> {
        if self.finished {
            return Vec::new();
        }
        let mut events = self.ensure_created();
        self.finished = true;
        let mut response = self.response_object("failed");
        response["error"] = json!({"code": code, "message": message});
        events.push(json!({"type": "response.failed", "response": response}));
        events
    }

    fn ensure_created(&mut self) -> Vec<Value> {
        if self.created {
            return Vec::new();
        }
        self.created = true;
        vec![json!({
            "type": "response.created",
            "response": {
                "id": self.response_id(),
                "object": "response",
                "status": "in_progress",
                "model": self.model,
                "output": [],
            },
        })]
    }

    fn response_object(&self, status: &str) -> Value {
        json!({
            "id": self.response_id(),
            "object": "response",
            "status": status,
            "model": self.model,
            "output": self.output,
            "usage": self.usage.to_responses(),
        })
    }

    fn start_block(&mut self, index: u64, block: &Value) -> Vec<Value> {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if is_server_block_type(block_type) {
            self.server_blocks.insert(
                index,
                ServerBlock {
                    block: block.clone(),
                    input_json: String::new(),
                },
            );
            return Vec::new();
        }
        // Anything else ends a run of server tool blocks.
        let mut events = self.flush_server_run();
        let (prefix, state) = match block_type {
            "text" => (
                "msg",
                BlockState::Text {
                    text: String::new(),
                },
            ),
            "thinking" => (
                "rs",
                BlockState::Thinking {
                    thinking: String::new(),
                    signature: block
                        .get("signature")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    part_added: false,
                },
            ),
            "redacted_thinking" => (
                "rs",
                BlockState::RedactedThinking {
                    data: block
                        .get("data")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
            ),
            "tool_use" => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let target = self.tools.resolve(name);
                let prefix = match target.kind {
                    ToolKind::Function => "fc",
                    ToolKind::Custom => "ctc",
                };
                (
                    prefix,
                    BlockState::ToolUse {
                        call_id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        target,
                        arguments: String::new(),
                        initial_input: block
                            .get("input")
                            .filter(|input| {
                                input.as_object().is_some_and(|input| !input.is_empty())
                            })
                            .cloned(),
                    },
                )
            }
            other => {
                debug!(block_type = other, "ignoring Anthropic content block");
                return events;
            }
        };
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let item_id = format!("{prefix}_{}_{index}", self.id_stem);
        let item = match &state {
            BlockState::Text { .. } => json!({
                "type": "message",
                "id": item_id,
                "role": "assistant",
                "status": "in_progress",
                "content": [],
            }),
            BlockState::Thinking { .. } | BlockState::RedactedThinking { .. } => json!({
                "type": "reasoning",
                "id": item_id,
                "summary": [],
            }),
            BlockState::ToolUse {
                call_id, target, ..
            } => call_item(&item_id, call_id, target, "", "in_progress"),
        };
        events.push(json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item,
        }));
        self.blocks.insert(
            index,
            OpenBlock {
                output_index,
                item_id,
                state,
            },
        );
        // Non-streaming style blocks may arrive with their text up front.
        if block_type == "text"
            && let Some(text) = block
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        {
            events.extend(self.apply_delta(index, &json!({"type": "text_delta", "text": text})));
        }
        if block_type == "thinking"
            && let Some(thinking) = block
                .get("thinking")
                .and_then(Value::as_str)
                .filter(|thinking| !thinking.is_empty())
        {
            events.extend(self.apply_delta(
                index,
                &json!({"type": "thinking_delta", "thinking": thinking}),
            ));
        }
        events
    }

    fn apply_delta(&mut self, index: u64, delta: &Value) -> Vec<Value> {
        if let Some(server) = self.server_blocks.get_mut(&index) {
            match delta.get("type").and_then(Value::as_str) {
                Some("input_json_delta") => server.input_json.push_str(
                    delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                other => debug!(delta_type = ?other, "ignoring server tool block delta"),
            }
            return Vec::new();
        }
        let Some(block) = self.blocks.get_mut(&index) else {
            return Vec::new();
        };
        let delta_type = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let output_index = block.output_index;
        let item_id = block.item_id.clone();
        match (&mut block.state, delta_type) {
            (BlockState::Text { text }, "text_delta") => {
                let chunk = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if chunk.is_empty() {
                    return Vec::new();
                }
                text.push_str(chunk);
                vec![json!({
                    "type": "response.output_text.delta",
                    "item_id": item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "delta": chunk,
                })]
            }
            (
                BlockState::Thinking {
                    thinking,
                    part_added,
                    ..
                },
                "thinking_delta",
            ) => {
                let chunk = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if chunk.is_empty() {
                    return Vec::new();
                }
                thinking.push_str(chunk);
                let mut events = Vec::new();
                if !*part_added {
                    *part_added = true;
                    events.push(json!({
                        "type": "response.reasoning_summary_part.added",
                        "item_id": item_id,
                        "output_index": output_index,
                        "summary_index": 0,
                        "part": {"type": "summary_text", "text": ""},
                    }));
                }
                events.push(json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": item_id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "delta": chunk,
                }));
                events
            }
            (BlockState::Thinking { signature, .. }, "signature_delta") => {
                signature.push_str(
                    delta
                        .get("signature")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                Vec::new()
            }
            (
                BlockState::ToolUse {
                    arguments, target, ..
                },
                "input_json_delta",
            ) => {
                let chunk = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                arguments.push_str(chunk);
                // Freeform input is only known once the JSON wrapper is complete.
                if target.kind == ToolKind::Function && !chunk.is_empty() {
                    vec![json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": item_id,
                        "output_index": output_index,
                        "delta": chunk,
                    })]
                } else {
                    Vec::new()
                }
            }
            (_, other) => {
                debug!(delta_type = other, "ignoring Anthropic content delta");
                Vec::new()
            }
        }
    }

    fn stop_block(&mut self, index: u64) -> Vec<Value> {
        if let Some(server) = self.server_blocks.remove(&index) {
            return self.stop_server_block(index, server);
        }
        let Some(block) = self.blocks.remove(&index) else {
            return Vec::new();
        };
        let OpenBlock {
            output_index,
            item_id,
            state,
        } = block;
        let mut events = Vec::new();
        let item = match state {
            BlockState::Text { text } => json!({
                "type": "message",
                "id": item_id,
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            }),
            BlockState::Thinking {
                thinking,
                signature,
                part_added,
            } => {
                if part_added {
                    events.push(json!({
                        "type": "response.reasoning_summary_text.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "summary_index": 0,
                        "text": thinking,
                    }));
                    events.push(json!({
                        "type": "response.reasoning_summary_part.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "summary_index": 0,
                        "part": {"type": "summary_text", "text": thinking},
                    }));
                }
                let summary = if thinking.is_empty() {
                    json!([])
                } else {
                    json!([{"type": "summary_text", "text": thinking}])
                };
                let encrypted_content = if signature.is_empty() {
                    Value::Null
                } else {
                    Value::String(encode_thinking(&thinking, &signature))
                };
                json!({
                    "type": "reasoning",
                    "id": item_id,
                    "summary": summary,
                    "encrypted_content": encrypted_content,
                })
            }
            BlockState::RedactedThinking { data } => json!({
                "type": "reasoning",
                "id": item_id,
                "summary": [],
                "encrypted_content": encode_redacted_thinking(&data),
            }),
            BlockState::ToolUse {
                call_id,
                target,
                arguments,
                initial_input,
            } => {
                let arguments = if arguments.trim().is_empty() {
                    initial_input.map_or_else(|| "{}".to_string(), |input| input.to_string())
                } else {
                    arguments
                };
                match target.kind {
                    ToolKind::Function => {
                        call_item(&item_id, &call_id, &target, &arguments, "completed")
                    }
                    ToolKind::Custom => {
                        let input = custom_tool_input(&arguments);
                        if !input.is_empty() {
                            events.push(json!({
                                "type": "response.custom_tool_call_input.delta",
                                "item_id": item_id,
                                "call_id": call_id,
                                "output_index": output_index,
                                "delta": input,
                            }));
                        }
                        call_item(&item_id, &call_id, &target, &input, "completed")
                    }
                }
            }
        };
        self.output.push(item.clone());
        events.push(json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }));
        events
    }

    /// Finishes a server tool block: records it for replay and updates the
    /// `web_search_call` item Codex shows.
    fn stop_server_block(&mut self, index: u64, server: ServerBlock) -> Vec<Value> {
        let ServerBlock {
            mut block,
            input_json,
        } = server;
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut events = Vec::new();
        if block_type == "server_tool_use" {
            if !input_json.trim().is_empty() {
                block["input"] = serde_json::from_str::<Value>(&input_json)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
            }
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if block.get("name").and_then(Value::as_str) == Some("web_search") {
                let query = block
                    .get("input")
                    .and_then(|input| input.get("query"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let search = WebSearch {
                    item_id: format!("ws_{}_{index}", self.id_stem),
                    output_index: self.next_output_index,
                    query,
                };
                self.next_output_index += 1;
                events.push(json!({
                    "type": "response.output_item.added",
                    "output_index": search.output_index,
                    "item": web_search_item(&search, "in_progress"),
                }));
                self.searches.insert(id, search);
            }
        } else if let Some(search) = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .and_then(|id| self.searches.remove(id))
        {
            // Errors arrive as HTTP 200 with an error object instead of a list.
            let status = if block.get("content").is_some_and(Value::is_array) {
                "completed"
            } else {
                "failed"
            };
            events.extend(self.finish_search(&search, status));
        }
        if self.server_run.is_empty() {
            self.server_run_start = index;
        }
        self.server_run.push(block);
        events
    }

    fn finish_search(&mut self, search: &WebSearch, status: &str) -> Vec<Value> {
        let item = web_search_item(search, status);
        self.output.push(item.clone());
        vec![json!({
            "type": "response.output_item.done",
            "output_index": search.output_index,
            "item": item,
        })]
    }

    /// Stores the pending server tool blocks in a reasoning item, the one
    /// item kind whose `encrypted_content` Codex sends back to custom
    /// providers, so the next request replays them in place.
    fn flush_server_run(&mut self) -> Vec<Value> {
        if self.server_run.is_empty() {
            return Vec::new();
        }
        let blocks = std::mem::take(&mut self.server_run);
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let item_id = format!("rs_{}_{}", self.id_stem, self.server_run_start);
        let item = json!({
            "type": "reasoning",
            "id": item_id,
            "summary": [],
            "encrypted_content": encode_server_blocks(&blocks),
        });
        self.output.push(item.clone());
        vec![
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {"type": "reasoning", "id": item_id, "summary": []},
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item,
            }),
        ]
    }

    fn complete(&mut self) -> Vec<Value> {
        let mut events = self.ensure_created();
        let mut open: Vec<u64> = self
            .blocks
            .keys()
            .chain(self.server_blocks.keys())
            .copied()
            .collect();
        open.sort_unstable();
        for index in open {
            events.extend(self.stop_block(index));
        }
        let unfinished: Vec<WebSearch> = self.searches.drain().map(|(_, search)| search).collect();
        for search in unfinished {
            events.extend(self.finish_search(&search, "failed"));
        }
        events.extend(self.flush_server_run());
        let terminal = match self.stop_reason.as_deref() {
            Some("max_tokens") => {
                let mut response = self.response_object("incomplete");
                response["incomplete_details"] = json!({"reason": "max_output_tokens"});
                json!({"type": "response.incomplete", "response": response})
            }
            Some("refusal") => {
                // A non-retryable failure that says why, not an empty success.
                let mut response = self.response_object("failed");
                response["error"] = json!({
                    "code": "invalid_prompt",
                    "message": refusal_message(self.stop_details.as_ref()),
                });
                json!({"type": "response.failed", "response": response})
            }
            Some("model_context_window_exceeded") => {
                let mut response = self.response_object("failed");
                response["error"] = json!({
                    "code": "context_length_exceeded",
                    "message": "The conversation exceeded the model context window.",
                });
                json!({"type": "response.failed", "response": response})
            }
            stop_reason => {
                let mut response = self.response_object("completed");
                // `pause_turn` asks the client to send the conversation back.
                if stop_reason == Some("pause_turn") {
                    response["end_turn"] = Value::Bool(false);
                }
                json!({"type": "response.completed", "response": response})
            }
        };
        self.finished = true;
        events.push(terminal);
        events
    }
}

fn web_search_item(search: &WebSearch, status: &str) -> Value {
    json!({
        "type": "web_search_call",
        "id": search.item_id,
        "status": status,
        "action": {"type": "search", "query": search.query},
    })
}

/// Describes `stop_reason: "refusal"` from its `stop_details`
/// (`{type: "refusal", category, explanation}`).
fn refusal_message(stop_details: Option<&Value>) -> String {
    let field = |key: &str| {
        stop_details
            .and_then(|details| details.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    };
    let category = field("category").unwrap_or("unspecified");
    let mut message =
        format!("Claude declined to continue this request (refusal category: {category}).");
    if let Some(explanation) = field("explanation") {
        message.push(' ');
        message.push_str(explanation);
    }
    message
}

fn call_item(
    item_id: &str,
    call_id: &str,
    target: &ToolTarget,
    payload: &str,
    status: &str,
) -> Value {
    let mut item = Map::new();
    let (item_type, payload_key) = match target.kind {
        ToolKind::Function => ("function_call", "arguments"),
        ToolKind::Custom => ("custom_tool_call", "input"),
    };
    item.insert("type".to_string(), json!(item_type));
    item.insert("id".to_string(), json!(item_id));
    item.insert("call_id".to_string(), json!(call_id));
    item.insert("name".to_string(), json!(target.name));
    if let Some(namespace) = &target.namespace {
        item.insert("namespace".to_string(), json!(namespace));
    }
    item.insert(payload_key.to_string(), json!(payload));
    item.insert("status".to_string(), json!(status));
    Value::Object(item)
}

/// Extracts the raw text of a freeform tool call from its JSON wrapper.
fn custom_tool_input(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(object)) => match object.get(CUSTOM_TOOL_INPUT_PROPERTY) {
            Some(Value::String(input)) => input.clone(),
            Some(other) => other.to_string(),
            // Tolerate a single differently named string property.
            None => match object.values().collect::<Vec<_>>().as_slice() {
                [Value::String(input)] => input.clone(),
                _ => arguments.to_string(),
            },
        },
        Ok(Value::String(input)) => input,
        _ => arguments.to_string(),
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
