use super::*;
use crate::reasoning::encode_thinking;
use crate::tools::ToolKind;
use crate::tools::ToolTarget;
use pretty_assertions::assert_eq;

fn translate(request: Value) -> (Value, ToolRegistry) {
    let translated = translate_request(&request, &TranslateOptions { max_tokens: 32_000 })
        .expect("translation succeeds");
    let body = serde_json::to_string(&translated.body).expect("serialize request");
    (
        serde_json::from_str(&body).expect("valid JSON"),
        translated.tools,
    )
}

fn user_text(text: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
}

#[test]
fn translates_a_full_codex_turn() {
    let thinking = encode_thinking("I should list files.", "sig-1");
    let request = json!({
        "model": "claude-opus-5-5",
        "stream": true,
        "store": false,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "thread-1",
        "reasoning": {"effort": "high", "summary": "auto"},
        "text": {"format": {"type": "json_schema", "name": "out", "strict": true, "schema": {"type": "object"}}},
        "tools": [
            {
                "type": "function",
                "name": "shell_command",
                "description": "Runs a shell command.",
                "strict": false,
                "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]},
            },
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Edit files.",
                "format": {"type": "grammar", "syntax": "lark", "definition": "start: patch"},
            },
            {
                "type": "namespace",
                "name": "mcp__github",
                "description": "GitHub tools.",
                "tools": [{
                    "type": "function",
                    "name": "create_issue",
                    "description": "Create an issue.",
                    "strict": false,
                    "parameters": {"type": "object", "properties": {"title": {"type": "string"}}},
                }],
            },
            {"type": "web_search", "external_web_access": true},
        ],
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are Codex."}]},
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Sandbox: workspace-write."}]},
            user_text("<environment_context>cwd=/repo</environment_context>"),
            user_text("List the files."),
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "I should list files."}], "encrypted_content": thinking},
            {"type": "message", "role": "assistant", "id": "msg_1", "content": [{"type": "output_text", "text": "Listing."}]},
            {"type": "function_call", "id": "fc_1", "name": "shell_command", "arguments": "{\"command\":\"ls\"}", "call_id": "toolu_1"},
            {"type": "function_call", "id": "fc_2", "name": "create_issue", "namespace": "mcp__github", "arguments": "{\"title\":\"x\"}", "call_id": "toolu_2"},
            {"type": "function_call_output", "call_id": "toolu_1", "output": "a.txt\nb.txt"},
            {"type": "function_call_output", "call_id": "toolu_2", "output": [
                {"type": "input_text", "text": "created"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
            ]},
            {"type": "custom_tool_call", "id": "ctc_1", "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch", "call_id": "toolu_3"},
            {"type": "custom_tool_call_output", "call_id": "toolu_3", "output": "Done!"},
            {"type": "web_search_call", "id": "ws_1", "status": "completed"},
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Approval policy changed."}]},
            user_text("Thanks"),
        ],
    });

    let (body, registry) = translate(request);

    assert_eq!(
        body,
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 32000,
            "stream": true,
            "system": [
                {"type": "text", "text": "You are Codex."},
                {"type": "text", "text": "Sandbox: workspace-write."},
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "<environment_context>cwd=/repo</environment_context>"},
                    {"type": "text", "text": "List the files."},
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "I should list files.", "signature": "sig-1"},
                    {"type": "text", "text": "Listing."},
                    {"type": "tool_use", "id": "toolu_1", "name": "shell_command", "input": {"command": "ls"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "mcp__github__create_issue", "input": {"title": "x"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt\nb.txt"},
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": [
                        {"type": "text", "text": "created"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                    ]},
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_3", "name": "apply_patch", "input": {"input": "*** Begin Patch\n*** End Patch"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_3", "content": "Done!"},
                    {"type": "text", "text": "<developer_message>\nApproval policy changed.\n</developer_message>"},
                    {"type": "text", "text": "Thanks"},
                ]},
            ],
            "tools": [
                {
                    "name": "shell_command",
                    "description": "Runs a shell command.",
                    "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]},
                },
                {
                    "name": "apply_patch",
                    "description": "Edit files.\n\nThis is a freeform tool: put the complete raw input text in the `input` string property. The input must conform to this lark grammar:\n```\nstart: patch\n```",
                    "input_schema": {
                        "type": "object",
                        "properties": {"input": {"type": "string", "description": "The complete raw tool input as plain text."}},
                        "required": ["input"],
                    },
                },
                {
                    "name": "mcp__github__create_issue",
                    "description": "Create an issue.",
                    "input_schema": {"type": "object", "properties": {"title": {"type": "string"}}},
                },
            ],
            "tool_choice": {"type": "auto"},
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": "high", "format": {"type": "json_schema", "schema": {"type": "object"}}},
            "cache_control": {"type": "ephemeral"},
        })
    );

    assert_eq!(
        registry.lookup("mcp__github__create_issue"),
        Some(&ToolTarget {
            kind: ToolKind::Function,
            name: "create_issue".to_string(),
            namespace: Some("mcp__github".to_string()),
        })
    );
    assert_eq!(
        registry.lookup("apply_patch").map(|target| target.kind),
        Some(ToolKind::Custom)
    );
}

#[test]
fn replays_tool_arguments_verbatim() {
    let translated = translate_request(
        &json!({
            "model": "claude-sonnet-5-5",
            "input": [
                user_text("go"),
                {"type": "function_call", "name": "f", "arguments": "{\"zeta\":1,\"alpha\":{\"b\":2,\"a\":3}}", "call_id": "toolu_9"},
                {"type": "function_call_output", "call_id": "toolu_9", "output": "ok"},
            ],
        }),
        &TranslateOptions::default(),
    )
    .expect("translation succeeds");
    let serialized = serde_json::to_string(&translated.body).expect("serialize");
    assert!(
        serialized.contains(r#""input":{"zeta":1,"alpha":{"b":2,"a":3}}"#),
        "{serialized}"
    );
    let body: Value = serde_json::from_str(&serialized).expect("valid JSON");
    assert_eq!(body["messages"][1]["content"][0]["id"], json!("toolu_9"));
    // No tools declared: no tool_choice.
    assert_eq!(body.get("tool_choice"), None);
}

#[test]
fn repairs_tool_pairing_and_turn_order() {
    let (body, _) = translate(json!({
        "model": "claude-opus-5-5",
        "input": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Earlier answer."}]},
            {"type": "function_call", "name": "f", "arguments": "", "call_id": "toolu_a"},
            user_text("interrupted"),
            {"type": "function_call_output", "call_id": "toolu_orphan", "output": "late"},
            {"type": "function_call", "name": "f", "arguments": "[1]", "call_id": "call.b"},
        ],
    }));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Continue."}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Earlier answer."},
                {"type": "tool_use", "id": "toolu_a", "name": "f", "input": {}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_a", "is_error": true, "content": "No output was recorded for this tool call."},
                {"type": "text", "text": "interrupted"},
                {"type": "text", "text": "Tool output for call toolu_orphan:\nlate"},
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_b", "name": "f", "input": {"value": [1]}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_b", "is_error": true, "content": "No output was recorded for this tool call."},
            ]},
        ])
    );
}

#[test]
fn drops_unsigned_or_foreign_reasoning_and_trailing_assistant_gets_a_user_turn() {
    let (body, _) = translate(json!({
        "model": "claude-opus-5-5",
        "input": [
            user_text("hi"),
            {"type": "reasoning", "summary": [], "encrypted_content": "gAAAA-openai"},
            {"type": "reasoning", "summary": [], "encrypted_content": null},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hello"}]},
        ],
    }));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "Hello"}]},
            {"role": "user", "content": [{"type": "text", "text": "Continue."}]},
        ])
    );
}

#[test]
fn string_input_and_missing_model() {
    let (body, _) = translate(json!({"model": "claude-haiku-5-5", "input": "hello"}));
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])
    );
    assert_eq!(
        translate_request(&json!({"input": []}), &TranslateOptions::default()).map(|_| ()),
        Err(TranslateError("request is missing `model`".to_string()))
    );
}

#[test]
fn parallel_tool_calls_false_disables_parallel_use() {
    let (body, _) = translate(json!({
        "model": "claude-opus-5-5",
        "parallel_tool_calls": false,
        "tools": [{"type": "function", "name": "f", "parameters": {"type": "object", "properties": {}}}],
        "input": [user_text("hi")],
    }));
    assert_eq!(
        body["tool_choice"],
        json!({"type": "auto", "disable_parallel_tool_use": true})
    );
}

#[test]
fn chooses_thinking_style_by_model() {
    let adaptive = ThinkingStyle::Adaptive {
        supports_xhigh: true,
    };
    for model in [
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-fable-5-2",
        "claude-opus-4-7",
        "anthropic.claude-opus-5-5",
        "some-custom-alias",
    ] {
        assert_eq!(thinking_style(model), adaptive, "{model}");
    }
    assert_eq!(
        thinking_style("claude-sonnet-4-6"),
        ThinkingStyle::Adaptive {
            supports_xhigh: false
        }
    );
    for model in [
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
        "claude-opus-4-1-20250805",
        "claude-sonnet-4-20250514",
        "claude-3-7-sonnet-latest",
    ] {
        assert_eq!(thinking_style(model), ThinkingStyle::Budget, "{model}");
    }
    assert_eq!(
        thinking_style("claude-3-5-haiku-latest"),
        ThinkingStyle::Unsupported
    );
}

#[test]
fn maps_effort_for_each_thinking_style() {
    let reasoning = |effort: &str| json!({"effort": effort, "summary": "auto"});
    assert_eq!(
        plan_thinking("claude-opus-5-5", Some(&reasoning("none")), 1_000),
        ThinkingPlan {
            thinking: Some(json!({"type": "adaptive", "display": "summarized"})),
            effort: Some("low"),
            max_tokens: 1_000,
        }
    );
    assert_eq!(
        plan_thinking("claude-sonnet-4-6", Some(&reasoning("xhigh")), 1_000).effort,
        Some("max")
    );
    assert_eq!(
        plan_thinking(
            "claude-opus-5-5",
            Some(&json!({"effort": "medium", "summary": "none"})),
            1_000
        )
        .thinking,
        Some(json!({"type": "adaptive", "display": "omitted"}))
    );
    assert_eq!(
        plan_thinking("claude-sonnet-4-5", Some(&reasoning("high")), 1_000),
        ThinkingPlan {
            thinking: Some(json!({"type": "enabled", "budget_tokens": 16_384})),
            effort: None,
            max_tokens: 16_384 + 4_096,
        }
    );
    assert_eq!(
        plan_thinking("claude-sonnet-4-5", Some(&reasoning("none")), 1_000).thinking,
        None
    );
    assert_eq!(
        plan_thinking("claude-sonnet-4-5", None, 1_000).thinking,
        None
    );
}
