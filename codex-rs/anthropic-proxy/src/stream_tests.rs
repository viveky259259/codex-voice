use super::*;
use crate::tools::translate_tools;
use pretty_assertions::assert_eq;

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    translate_tools(
        &[
            json!({
                "type": "namespace",
                "name": "mcp__github",
                "description": "",
                "tools": [{"type": "function", "name": "create_issue", "description": "", "parameters": {"type": "object"}}],
            }),
            json!({
                "type": "custom",
                "name": "apply_patch",
                "description": "",
                "format": {"type": "grammar", "syntax": "lark", "definition": ""},
            }),
        ],
        &mut registry,
        "web_search_20260209",
    );
    registry
}

fn run(events: &[Value]) -> Vec<Value> {
    let mut translator = StreamTranslator::new(registry(), "requested-model".to_string());
    let mut out = Vec::new();
    for event in events {
        out.extend(translator.on_event(event));
    }
    out.extend(translator.finish());
    out
}

fn message_start() -> Value {
    json!({
        "type": "message_start",
        "message": {
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5-5",
            "content": [],
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 100, "output_tokens": 1},
        },
    })
}

fn created() -> Value {
    json!({
        "type": "response.created",
        "response": {"id": "resp_01", "object": "response", "status": "in_progress", "model": "claude-opus-5-5", "output": []},
    })
}

#[test]
fn translates_thinking_text_and_tool_calls() {
    let events = run(&[
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "ping"}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Plan"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": " it."}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig=="}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hi"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "mcp__github__create_issue", "input": {}}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"title\":"}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"x\"}"}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "toolu_2", "name": "apply_patch", "input": {}}}),
        json!({"type": "content_block_delta", "index": 3, "delta": {"type": "input_json_delta", "partial_json": "{\"input\":\"*** Begin Patch\\n\"}"}}),
        json!({"type": "content_block_stop", "index": 3}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 50}}),
        json!({"type": "message_stop"}),
    ]);

    let reasoning = json!({
        "type": "reasoning",
        "id": "rs_01_0",
        "summary": [{"type": "summary_text", "text": "Plan it."}],
        "encrypted_content": encode_thinking("Plan it.", "sig=="),
    });
    let message = json!({
        "type": "message",
        "id": "msg_01_1",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": "Hi", "annotations": []}],
    });
    let function_call = json!({
        "type": "function_call",
        "id": "fc_01_2",
        "call_id": "toolu_1",
        "name": "create_issue",
        "namespace": "mcp__github",
        "arguments": "{\"title\":\"x\"}",
        "status": "completed",
    });
    let custom_call = json!({
        "type": "custom_tool_call",
        "id": "ctc_01_3",
        "call_id": "toolu_2",
        "name": "apply_patch",
        "input": "*** Begin Patch\n",
        "status": "completed",
    });
    assert_eq!(
        events,
        vec![
            created(),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_01_0", "summary": []}}),
            json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_01_0", "output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_01_0", "output_index": 0, "summary_index": 0, "delta": "Plan"}),
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_01_0", "output_index": 0, "summary_index": 0, "delta": " it."}),
            json!({"type": "response.reasoning_summary_text.done", "item_id": "rs_01_0", "output_index": 0, "summary_index": 0, "text": "Plan it."}),
            json!({"type": "response.reasoning_summary_part.done", "item_id": "rs_01_0", "output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": "Plan it."}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "msg_01_1", "role": "assistant", "status": "in_progress", "content": []}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_01_1", "output_index": 1, "content_index": 0, "delta": "Hi"}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": message}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": {
                "type": "function_call", "id": "fc_01_2", "call_id": "toolu_1", "name": "create_issue",
                "namespace": "mcp__github", "arguments": "", "status": "in_progress",
            }}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_01_2", "output_index": 2, "delta": "{\"title\":"}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_01_2", "output_index": 2, "delta": "\"x\"}"}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": function_call}),
            json!({"type": "response.output_item.added", "output_index": 3, "item": {
                "type": "custom_tool_call", "id": "ctc_01_3", "call_id": "toolu_2", "name": "apply_patch",
                "input": "", "status": "in_progress",
            }}),
            json!({"type": "response.custom_tool_call_input.delta", "item_id": "ctc_01_3", "call_id": "toolu_2", "output_index": 3, "delta": "*** Begin Patch\n"}),
            json!({"type": "response.output_item.done", "output_index": 3, "item": custom_call}),
            json!({"type": "response.completed", "response": {
                "id": "resp_01",
                "object": "response",
                "status": "completed",
                "model": "claude-opus-5-5",
                "output": [reasoning, message, function_call, custom_call],
                "usage": {
                    "input_tokens": 115,
                    "input_tokens_details": {"cached_tokens": 100, "cache_write_tokens": 5},
                    "output_tokens": 50,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": 165,
                },
            }}),
        ]
    );
}

#[test]
fn max_tokens_becomes_incomplete() {
    let events = run(&[
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "cut"}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 7}}),
        json!({"type": "message_stop"}),
    ]);
    let last = events.last().cloned().unwrap_or_default();
    assert_eq!(last["type"], json!("response.incomplete"));
    assert_eq!(
        last["response"]["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );
    // The open text block is closed before the terminal event.
    assert_eq!(
        events[events.len() - 2]["type"],
        json!("response.output_item.done")
    );
    assert_eq!(
        events[events.len() - 2]["item"]["content"][0]["text"],
        json!("cut")
    );
}

#[test]
fn stream_errors_become_failed_responses() {
    let events = run(&[
        message_start(),
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        json!({"type": "message_stop"}),
    ]);
    assert_eq!(
        events,
        vec![
            created(),
            json!({"type": "response.failed", "response": {
                "id": "resp_01",
                "object": "response",
                "status": "failed",
                "model": "claude-opus-5-5",
                "output": [],
                "usage": {
                    "input_tokens": 115,
                    "input_tokens_details": {"cached_tokens": 100, "cache_write_tokens": 5},
                    "output_tokens": 1,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": 116,
                },
                "error": {"code": "server_is_overloaded", "message": "Anthropic API error (overloaded_error): Overloaded"},
            }}),
        ]
    );
}

#[test]
fn truncated_stream_fails() {
    let events = run(&[message_start()]);
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["type"], json!("response.failed"));
    assert_eq!(
        events[1]["response"]["error"],
        json!({"code": "server_error", "message": "Anthropic stream ended before message_stop"})
    );
}

#[test]
fn redacted_thinking_and_refusal() {
    let events = run(&[
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "redacted_thinking", "data": "opaque"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {
            "stop_reason": "refusal",
            "stop_details": {"type": "refusal", "category": "cyber", "explanation": "This looks like malware development."},
        }, "usage": {"output_tokens": 2}}),
        json!({"type": "message_stop"}),
    ]);
    assert_eq!(
        events[2],
        json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "type": "reasoning",
            "id": "rs_01_0",
            "summary": [],
            "encrypted_content": encode_redacted_thinking("opaque"),
        }})
    );
    assert_eq!(events[3]["type"], json!("response.failed"));
    assert_eq!(
        events[3]["response"]["error"],
        json!({
            "code": "invalid_prompt",
            "message": "Claude declined to continue this request (refusal category: cyber). This looks like malware development.",
        })
    );
}

#[test]
fn refusal_without_details_still_fails() {
    let events = run(&[
        message_start(),
        json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": null}}),
        json!({"type": "message_stop"}),
    ]);
    let last = events.last().cloned().unwrap_or_default();
    assert_eq!(last["type"], json!("response.failed"));
    assert_eq!(
        last["response"]["error"]["message"],
        json!("Claude declined to continue this request (refusal category: unspecified).")
    );
}

#[test]
fn web_search_becomes_a_call_item_and_a_replayable_reasoning_item() {
    let results = json!([{
        "type": "web_search_result",
        "url": "https://docs.rs/serde",
        "title": "serde - Rust",
        "encrypted_content": "EqgfCioIARgB",
        "page_age": null,
    }]);
    let events = run(&[
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\": "}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "\"serde docs\"}"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": results}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": "", "citations": []}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "citations_delta", "citation": {"type": "web_search_result_location", "url": "https://docs.rs/serde"}}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "See docs.rs."}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}}),
        json!({"type": "message_stop"}),
    ]);
    let search = |status: &str| json!({"type": "web_search_call", "id": "ws_01_0", "status": status, "action": {"type": "search", "query": "serde docs"}});
    let carrier = json!({
        "type": "reasoning",
        "id": "rs_01_0",
        "summary": [],
        "encrypted_content": encode_server_blocks(&[
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "serde docs"}}),
            json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": results}),
        ]),
    });
    let item_events: Vec<Value> = events
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("response.output_item.added" | "response.output_item.done")
            )
        })
        .cloned()
        .collect();
    assert_eq!(
        item_events,
        vec![
            json!({"type": "response.output_item.added", "output_index": 0, "item": search("in_progress")}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": search("completed")}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "reasoning", "id": "rs_01_0", "summary": []}}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": carrier}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "message", "id": "msg_01_2", "role": "assistant", "status": "in_progress", "content": []}}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": {
                "type": "message", "id": "msg_01_2", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "See docs.rs.", "annotations": []}],
            }}),
        ]
    );
    let completed = events.last().cloned().unwrap_or_default();
    assert_eq!(completed["type"], json!("response.completed"));
    assert_eq!(
        completed["response"]["output"].as_array().map(Vec::len),
        Some(3)
    );
}

#[test]
fn web_search_errors_and_trailing_server_blocks() {
    let events = run(&[
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "q"}}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "pause_turn"}}),
        json!({"type": "message_stop"}),
    ]);
    let kinds: Vec<String> = events
        .iter()
        .map(|event| {
            format!(
                "{}:{}:{}",
                event["type"].as_str().unwrap_or_default(),
                event["item"]["type"].as_str().unwrap_or_default(),
                event["item"]["status"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "response.created::",
            "response.output_item.added:web_search_call:in_progress",
            "response.output_item.done:web_search_call:failed",
            "response.output_item.added:reasoning:",
            "response.output_item.done:reasoning:",
            "response.completed::",
        ]
    );
    assert_eq!(
        events.last().cloned().unwrap_or_default()["response"]["end_turn"],
        json!(false)
    );
}

#[test]
fn sse_frames_name_the_event() {
    let event = json!({"type": "response.output_text.delta", "delta": "a"});
    let frame = String::from_utf8(encode_sse(&event).to_vec()).expect("utf-8 frame");
    let data = frame
        .strip_prefix("event: response.output_text.delta\ndata: ")
        .and_then(|rest| rest.strip_suffix("\n\n"))
        .expect("well-formed SSE frame");
    assert_eq!(serde_json::from_str::<Value>(data).ok(), Some(event));
}

#[test]
fn custom_input_extraction_is_tolerant() {
    assert_eq!(custom_tool_input(r#"{"input":"raw"}"#), "raw");
    assert_eq!(custom_tool_input(r#"{"patch":"raw"}"#), "raw");
    assert_eq!(custom_tool_input("not json"), "not json");
}
