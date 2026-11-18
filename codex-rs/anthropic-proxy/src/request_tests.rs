use super::*;
use crate::models::ModelFamily;
use crate::models::model_family;
use crate::reasoning::encode_server_blocks;
use crate::reasoning::encode_thinking;
use crate::tools::ToolKind;
use crate::tools::ToolTarget;
use pretty_assertions::assert_eq;

fn translate(request: Value) -> (Value, ToolRegistry) {
    let options = TranslateOptions {
        max_tokens: 32_000,
        ..TranslateOptions::default()
    };
    let translated = translate_request(&request, &options).expect("translation succeeds");
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
                {"type": "text", "text": "Sandbox: workspace-write.", "cache_control": {"type": "ephemeral"}},
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
                {
                    "name": "shell_command",
                    "description": "Runs a shell command.",
                    "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]},
                },
                {"type": "web_search_20260209", "name": "web_search"},
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

fn reasoning(effort: &str) -> Value {
    json!({"effort": effort, "summary": "auto"})
}

fn adaptive(display: &str) -> Option<Value> {
    Some(json!({"type": "adaptive", "display": display}))
}

#[test]
fn thinking_rules_per_family() {
    let summarized = adaptive("summarized");
    // (model, reasoning, thinking, effort, max_tokens)
    let cases: Vec<(&str, Option<Value>, Option<Value>, Option<&str>, u32)> = vec![
        // Thinking cannot be turned off: `none` becomes the lowest effort.
        (
            "claude-fable-5-2",
            Some(reasoning("none")),
            summarized.clone(),
            Some("low"),
            1_000,
        ),
        (
            "claude-mythos-5-2",
            Some(reasoning("xhigh")),
            summarized.clone(),
            Some("xhigh"),
            1_000,
        ),
        (
            "claude-opus-5-5",
            Some(reasoning("none")),
            summarized.clone(),
            Some("low"),
            1_000,
        ),
        (
            "claude-opus-5-5",
            Some(reasoning("max")),
            summarized.clone(),
            Some("max"),
            1_000,
        ),
        (
            "claude-sonnet-5-5",
            Some(reasoning("none")),
            summarized.clone(),
            Some("low"),
            1_000,
        ),
        (
            "claude-sonnet-5-5",
            Some(reasoning("xhigh")),
            summarized.clone(),
            Some("xhigh"),
            1_000,
        ),
        (
            "claude-haiku-5-5",
            Some(reasoning("none")),
            summarized.clone(),
            Some("low"),
            1_000,
        ),
        (
            "claude-opus-5",
            Some(reasoning("none")),
            summarized.clone(),
            Some("low"),
            1_000,
        ),
        (
            "claude-sonnet-5",
            Some(reasoning("minimal")),
            summarized.clone(),
            Some("low"),
            1_000,
        ),
        (
            "claude-next-model",
            Some(reasoning("none")),
            summarized.clone(),
            Some("low"),
            1_000,
        ),
        // Omitting `thinking` turns it off on Opus 4.8/4.7 and the 4.6 family.
        (
            "claude-opus-4-8",
            Some(reasoning("none")),
            None,
            Some("low"),
            1_000,
        ),
        (
            "claude-opus-4-7",
            Some(reasoning("high")),
            summarized.clone(),
            Some("high"),
            1_000,
        ),
        (
            "claude-opus-4-6",
            Some(reasoning("none")),
            None,
            Some("low"),
            1_000,
        ),
        (
            "claude-sonnet-4-6",
            Some(reasoning("xhigh")),
            summarized.clone(),
            Some("max"),
            1_000,
        ),
        // Budget thinking for Haiku 4.5 and older.
        (
            "claude-haiku-4-5",
            Some(reasoning("high")),
            Some(json!({"type": "enabled", "budget_tokens": 16_384})),
            None,
            16_384 + 4_096,
        ),
        (
            "claude-haiku-4-5",
            Some(reasoning("none")),
            None,
            None,
            1_000,
        ),
        (
            "claude-3-5-haiku-latest",
            Some(reasoning("high")),
            None,
            None,
            1_000,
        ),
        // No `reasoning`: the model's own defaults.
        ("claude-opus-5-5", None, None, None, 1_000),
        ("claude-haiku-4-5", None, None, None, 1_000),
        // Codex omits `summary` when the user chose `none`.
        (
            "claude-opus-5-5",
            Some(json!({"effort": "medium"})),
            adaptive("omitted"),
            Some("medium"),
            1_000,
        ),
        (
            "claude-opus-5-5",
            Some(json!({"effort": "low", "summary": "none"})),
            adaptive("omitted"),
            Some("low"),
            1_000,
        ),
    ];
    for (model, reasoning, thinking, effort, max_tokens) in cases {
        let plan = plan_thinking(&model_rules(model), reasoning.as_ref(), 1_000);
        assert_eq!(
            plan,
            ThinkingPlan {
                thinking,
                effort,
                max_tokens
            },
            "{model} {reasoning:?}"
        );
    }
}

/// `{type: "disabled"}` and `budget_tokens` are 400s on the adaptive models;
/// the proxy must never send them there, for any Codex effort.
#[test]
fn adaptive_models_never_get_disabled_or_budget_thinking() {
    for model in [
        "claude-fable-5-2",
        "claude-fable-5-1",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-5",
        "claude-sonnet-4-6",
        "anthropic.claude-opus-5-5",
        "some-custom-alias",
    ] {
        for effort in [
            None,
            Some("none"),
            Some("minimal"),
            Some("low"),
            Some("medium"),
            Some("high"),
            Some("xhigh"),
            Some("max"),
        ] {
            for reasoning in [
                effort.map(|effort| json!({"effort": effort})),
                Some(json!({"effort": effort, "summary": "auto"})),
            ] {
                let (body, _) = translate(json!({
                    "model": model,
                    "reasoning": reasoning,
                    "input": [user_text("hi")],
                }));
                let kind = body["thinking"]["type"].as_str();
                assert!(
                    matches!(kind, None | Some("adaptive")),
                    "{model} {effort:?}: {body}"
                );
                assert_eq!(body["thinking"].get("budget_tokens"), None, "{model}");
                let effort = body["output_config"]["effort"].as_str();
                let valid = ["low", "medium", "high", "xhigh", "max"];
                assert!(
                    effort.is_none_or(|effort| valid.contains(&effort)),
                    "{model}"
                );
            }
        }
    }
}

fn function_tool(name: &str) -> Value {
    json!({"type": "function", "name": name, "parameters": {"type": "object", "properties": {}}})
}

#[test]
fn forced_tool_choice_per_family() {
    let forced_named = json!({"type": "function", "name": "f"});
    // (model, reasoning, tool_choice, expected tool_choice, instruction)
    let cases: Vec<(&str, Option<Value>, Value, Value, Option<&str>)> = vec![
        // Rejected outright by these models.
        (
            "claude-fable-5-2",
            None,
            json!("required"),
            json!({"type": "auto"}),
            Some("Respond by calling one of the available tools."),
        ),
        (
            "claude-opus-5-5",
            None,
            forced_named.clone(),
            json!({"type": "auto"}),
            Some("Respond by calling the `f` tool."),
        ),
        (
            "claude-sonnet-5-5",
            None,
            json!("required"),
            json!({"type": "auto"}),
            Some("Respond by calling one of the available tools."),
        ),
        (
            "claude-next-model",
            None,
            json!("required"),
            json!({"type": "auto"}),
            Some("Respond by calling one of the available tools."),
        ),
        // Thinking on (by default or requested) also rules out forced choice.
        (
            "claude-opus-5",
            None,
            json!("required"),
            json!({"type": "auto"}),
            Some("Respond by calling one of the available tools."),
        ),
        (
            "claude-haiku-4-5",
            Some(reasoning("high")),
            forced_named.clone(),
            json!({"type": "auto"}),
            Some("Respond by calling the `f` tool."),
        ),
        // Thinking off: forced choice is kept.
        (
            "claude-opus-4-8",
            Some(reasoning("none")),
            json!("required"),
            json!({"type": "any"}),
            None,
        ),
        (
            "claude-sonnet-4-6",
            None,
            forced_named.clone(),
            json!({"type": "tool", "name": "f"}),
            None,
        ),
        (
            "claude-haiku-4-5",
            None,
            json!("required"),
            json!({"type": "any"}),
            None,
        ),
        // `none` and `auto` are accepted everywhere.
        (
            "claude-opus-5-5",
            None,
            json!("none"),
            json!({"type": "none"}),
            None,
        ),
        (
            "claude-fable-5-2",
            None,
            json!("auto"),
            json!({"type": "auto"}),
            None,
        ),
        // A forced choice naming an undeclared tool falls back to auto.
        (
            "claude-sonnet-4-6",
            None,
            json!({"type": "function", "name": "missing"}),
            json!({"type": "auto"}),
            None,
        ),
    ];
    for (model, reasoning, choice, expected, instruction) in cases {
        let (body, _) = translate(json!({
            "model": model,
            "reasoning": reasoning,
            "tool_choice": choice,
            "tools": [function_tool("f")],
            "input": [user_text("go")],
        }));
        assert_eq!(body["tool_choice"], expected, "{model} {choice}");
        let mut expected_content = vec![json!({"type": "text", "text": "go"})];
        if let Some(instruction) = instruction {
            expected_content.push(json!({"type": "text", "text": instruction}));
        }
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": expected_content}]),
            "{model} {choice}"
        );
    }
}

#[test]
fn sampling_params_per_family() {
    // (model, reasoning, forwarded)
    for (model, reasoning, forwarded) in [
        ("claude-fable-5-2", None, false),
        ("claude-opus-5-5", None, false),
        ("claude-sonnet-5-5", None, false),
        ("claude-haiku-5-5", None, false),
        ("claude-opus-5", None, false),
        ("claude-sonnet-5", None, false),
        ("claude-opus-4-8", None, false),
        ("claude-opus-4-7", None, false),
        ("claude-next-model", None, false),
        // Accepted, but not together with thinking.
        ("claude-sonnet-4-6", None, true),
        ("claude-sonnet-4-6", Some(reasoning("high")), false),
        ("claude-haiku-4-5", None, true),
        ("claude-haiku-4-5", Some(reasoning("low")), false),
        ("claude-3-5-haiku-latest", Some(reasoning("high")), true),
    ] {
        let (body, _) = translate(json!({
            "model": model,
            "reasoning": reasoning,
            "temperature": 0.2,
            "top_p": 0.9,
            "input": [user_text("hi")],
        }));
        let expected = |value: f64| if forwarded { json!(value) } else { Value::Null };
        assert_eq!(body["temperature"], expected(0.2), "{model}");
        assert_eq!(body["top_p"], expected(0.9), "{model}");
    }
}

#[test]
fn families_cover_every_documented_model() {
    for model in [
        "claude-fable-5-2",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-5",
        "claude-sonnet-4-6",
        "claude-haiku-4-5",
    ] {
        assert_ne!(model_family(model), ModelFamily::Unknown, "{model}");
    }
}

#[test]
fn web_search_tool_version_per_family() {
    for (model, tool_type) in [
        ("claude-fable-5-2", "web_search_20260209"),
        ("claude-opus-5-5", "web_search_20260209"),
        ("claude-sonnet-5-5", "web_search_20260209"),
        ("claude-opus-5", "web_search_20260209"),
        ("claude-sonnet-5", "web_search_20260209"),
        ("claude-opus-4-8", "web_search_20260209"),
        ("claude-opus-4-6", "web_search_20260209"),
        ("claude-sonnet-4-6", "web_search_20260209"),
        ("claude-haiku-5-5", "web_search_20250305"),
        ("claude-haiku-4-5", "web_search_20250305"),
        ("claude-next-model", "web_search_20260209"),
    ] {
        let (body, _) = translate(json!({
            "model": model,
            "tools": [{"type": "web_search", "external_web_access": true}],
            "input": [user_text("hi")],
        }));
        assert_eq!(
            body["tools"],
            json!([{"type": tool_type, "name": "web_search"}]),
            "{model}"
        );
        assert_eq!(body["tool_choice"], json!({"type": "auto"}), "{model}");
    }
}

#[test]
fn replays_server_tool_blocks_from_reasoning_items() {
    let use_block = json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "q"}});
    let result_block = json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
        {"type": "web_search_result", "url": "https://example.com", "title": "Example", "encrypted_content": "abc"},
    ]});
    let dangling = json!({"type": "server_tool_use", "id": "srvtoolu_2", "name": "web_search", "input": {"query": "lost"}});
    let (body, _) = translate(json!({
        "model": "claude-opus-5-5",
        "input": [
            user_text("search"),
            {"type": "reasoning", "summary": [], "encrypted_content": encode_thinking("Search first.", "sig")},
            {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "q"}},
            {"type": "reasoning", "summary": [], "encrypted_content": encode_server_blocks(&[use_block.clone(), result_block.clone()])},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Found it."}]},
            {"type": "reasoning", "summary": [], "encrypted_content": encode_server_blocks(std::slice::from_ref(&dangling))},
            user_text("thanks"),
        ],
    }));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "search"}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Search first.", "signature": "sig"},
                use_block,
                result_block,
                {"type": "text", "text": "Found it."},
            ]},
            {"role": "user", "content": [{"type": "text", "text": "thanks"}]},
        ])
    );
}

/// A growing Codex conversation: each request is the previous one plus the
/// model's output and the next input.
fn conversation_request(turns: usize, tools: Vec<Value>) -> Value {
    let mut input = vec![
        json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are Codex."}]}),
        user_text("<environment_context>cwd=/repo</environment_context>"),
        user_text("Fix the bug."),
    ];
    for turn in 0..turns {
        let call_id = format!("toolu_{turn}");
        input.push(json!({"type": "reasoning", "summary": [], "encrypted_content": encode_thinking(&format!("step {turn}"), "sig")}));
        input.push(json!({"type": "function_call", "name": "shell_command", "arguments": format!("{{\"command\":\"cat {turn}\"}}"), "call_id": call_id}));
        input.push(json!({"type": "function_call_output", "call_id": call_id, "output": format!("output {turn}")}));
    }
    json!({
        "model": "claude-opus-5-5",
        "reasoning": {"effort": "high", "summary": "auto"},
        "tools": tools,
        "tool_choice": "auto",
        "input": input,
    })
}

fn codex_tools() -> Vec<Value> {
    vec![
        json!({"type": "function", "name": "shell_command", "description": "Run a command.", "parameters": {
            "type": "object",
            "properties": {"workdir": {"type": "string"}, "command": {"type": "string"}},
            "required": ["command"],
        }}),
        json!({"type": "custom", "name": "apply_patch", "description": "Edit files.", "format": {"type": "grammar", "syntax": "lark", "definition": "start: patch"}}),
        json!({"type": "web_search", "external_web_access": true}),
    ]
}

/// Serializes the request in Anthropic's render order (tools, system,
/// messages), each part with exactly the bytes sent on the wire.
fn render(body: &AnthropicRequest) -> Vec<String> {
    let mut parts = vec![encode(&body.tools), encode(&body.system)];
    parts.extend(body.messages.iter().map(encode));
    parts
}

fn encode<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("serialize")
}

#[test]
fn consecutive_requests_share_a_byte_identical_prefix() {
    let mut reordered = codex_tools();
    reordered.reverse();
    let options = TranslateOptions::default();
    let first = translate_request(&conversation_request(1, codex_tools()), &options)
        .expect("translation succeeds")
        .body;
    // The next request also lists Codex's tools in a different order.
    let second = translate_request(&conversation_request(2, reordered), &options)
        .expect("translation succeeds")
        .body;

    let first_parts = render(&first);
    let second_parts = render(&second);
    assert!(second_parts.len() > first_parts.len());
    // Tools, system and every message of the first request are byte-identical
    // at the start of the second, so its cache entry is a prefix hit.
    assert_eq!(first_parts[..], second_parts[..first_parts.len()]);

    let first_bytes = serde_json::to_string(&first).expect("serialize");
    let second_bytes = serde_json::to_string(&second).expect("serialize");
    for part in &first_parts {
        assert!(second_bytes.contains(part.as_str()), "missing {part}");
    }
    let first: Value = serde_json::from_str(&first_bytes).expect("valid JSON");
    let second: Value = serde_json::from_str(&second_bytes).expect("valid JSON");

    // Breakpoints: one explicit marker ending tools + system, plus the
    // automatic one at the end of the conversation. 2 of the 4 allowed.
    for (body, bytes) in [(&first, &first_bytes), (&second, &second_bytes)] {
        assert_eq!(bytes.matches("cache_control").count(), 2, "{bytes}");
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "You are Codex.", "cache_control": {"type": "ephemeral"}}])
        );
        let names: Vec<&str> = body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(names, vec!["apply_patch", "shell_command", "web_search"]);
    }
}

#[test]
fn cache_ttl_and_no_cache() {
    let request = conversation_request(1, codex_tools());
    let one_hour = TranslateOptions {
        cache_ttl: Some(CacheTtl::OneHour),
        ..TranslateOptions::default()
    };
    let body = serde_json::to_value(
        translate_request(&request, &one_hour)
            .expect("translation succeeds")
            .body,
    )
    .expect("serialize");
    let marker = json!({"type": "ephemeral", "ttl": "1h"});
    assert_eq!(body["cache_control"], marker);
    assert_eq!(body["system"][0]["cache_control"], marker);

    // Codex's own long-TTL hint upgrades the default to 1 hour.
    let mut long_ttl = request.clone();
    long_ttl["prompt_cache_options"] = json!({"ttl": "30m"});
    let (body, _) = translate(long_ttl);
    assert_eq!(body["cache_control"], marker);

    let no_cache = TranslateOptions {
        cache_ttl: None,
        ..TranslateOptions::default()
    };
    let bytes = serde_json::to_string(
        &translate_request(&request, &no_cache)
            .expect("translation succeeds")
            .body,
    )
    .expect("serialize");
    assert!(!bytes.contains("cache_control"), "{bytes}");
}

#[test]
fn stable_breakpoint_falls_back_to_the_last_client_tool() {
    let (body, _) = translate(json!({
        "model": "claude-opus-5-5",
        "tools": codex_tools(),
        "input": [user_text("hi")],
    }));
    assert_eq!(body.get("system"), None);
    assert_eq!(body["tools"][0].get("cache_control"), None);
    assert_eq!(
        body["tools"][1]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(body["tools"][2].get("cache_control"), None);
}
