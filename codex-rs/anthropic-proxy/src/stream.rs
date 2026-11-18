//! Translates an Anthropic Messages event stream into Responses API events.

use std::collections::BTreeMap;

use bytes::Bytes;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use tracing::debug;

use crate::reasoning::encode_redacted_thinking;
use crate::reasoning::encode_thinking;
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
    output: Vec<Value>,
    usage: Usage,
    stop_reason: Option<String>,
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
            output: Vec::new(),
            usage: Usage::default(),
            stop_reason: None,
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
                if let Some(stop_reason) = event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(stop_reason.to_string());
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
                // Server tools and their results have no Codex counterpart.
                debug!(block_type = other, "ignoring Anthropic content block");
                return Vec::new();
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
        let mut events = vec![json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": item,
        })];
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

    fn complete(&mut self) -> Vec<Value> {
        let mut events = self.ensure_created();
        let open: Vec<u64> = self.blocks.keys().copied().collect();
        for index in open {
            events.extend(self.stop_block(index));
        }
        let terminal = match self.stop_reason.as_deref() {
            Some("max_tokens") => {
                let mut response = self.response_object("incomplete");
                response["incomplete_details"] = json!({"reason": "max_output_tokens"});
                json!({"type": "response.incomplete", "response": response})
            }
            Some("refusal") => {
                let mut response = self.response_object("incomplete");
                response["incomplete_details"] = json!({"reason": "content_filter"});
                json!({"type": "response.incomplete", "response": response})
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
