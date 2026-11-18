//! End-to-end tests: Codex-shaped Responses requests go through the proxy to a
//! mock Anthropic server that replays canned Messages streams.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::post;
use codex_anthropic_proxy::ApiKey;
use codex_anthropic_proxy::ProxyConfig;
use codex_anthropic_proxy::serve;
use codex_api::AuthError;
use codex_api::AuthProvider;
use codex_api::Provider;
use codex_api::Reasoning;
use codex_api::ReqwestTransport;
use codex_api::ResponseEvent;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesApiTools;
use codex_api::ResponsesClient;
use codex_api::ResponsesOptions;
use codex_api::RetryConfig;
use codex_protocol::RequestCountry;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use serde_json::value::RawValue;
use tokio::net::TcpListener;

const FIRST_TURN: &str = include_str!("fixtures/thinking_text_tool_use.sse");
const SECOND_TURN: &str = include_str!("fixtures/final_answer.sse");
const SIGNATURE: &str = "EqQBCkYIBRgCKkBfixture+signature/==";

#[derive(Clone, Default)]
struct MockAnthropic {
    replies: Arc<Mutex<VecDeque<(StatusCode, String)>>>,
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

impl MockAnthropic {
    fn reply(&self, status: StatusCode, body: &str) {
        self.replies
            .lock()
            .unwrap()
            .push_back((status, body.to_string()));
    }

    fn requests(&self) -> Vec<(HeaderMap, Value)> {
        self.requests.lock().unwrap().clone()
    }
}

async fn messages(State(mock): State<MockAnthropic>, headers: HeaderMap, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap();
    mock.requests.lock().unwrap().push((headers, request));
    let (status, body) = mock
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .expect("a canned reply");
    let content_type = if status.is_success() {
        "text/event-stream"
    } else {
        "application/json"
    };
    (status, [("content-type", content_type)], body).into_response()
}

async fn start() -> (MockAnthropic, SocketAddr) {
    let mock = MockAnthropic::default();
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/messages", post(messages))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(upstream, app).await });

    let mut config = ProxyConfig::new(ApiKey::new("test-key".to_string()));
    config.anthropic_base_url = format!("http://{upstream_addr}");
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy.local_addr().unwrap();
    tokio::spawn(serve(proxy, config));
    (mock, proxy_addr)
}

async fn post_responses(addr: SocketAddr, body: &Value) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{addr}/v1/responses"))
        .json(body)
        .send()
        .await
        .unwrap()
}

/// Splits an SSE body into `(event name, data)` pairs.
fn parse_sse(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            let mut event = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    event = value.to_string();
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data.push_str(value);
                }
            }
            (event, serde_json::from_str(&data).unwrap())
        })
        .collect()
}

fn base_input() -> Vec<Value> {
    vec![
        json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are Codex, a coding agent."}]}),
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "List the files in /repo."}]}),
    ]
}

fn tools() -> Value {
    json!([{
        "type": "function",
        "name": "shell_command",
        "description": "Runs a shell command and returns its output.",
        "strict": false,
        "parameters": {
            "type": "object",
            "properties": {"command": {"type": "string"}, "workdir": {"type": "string"}},
            "required": ["command"],
            "additionalProperties": false,
        },
    }])
}

fn request_body(input: Vec<Value>) -> Value {
    json!({
        "model": "claude-opus-5-5",
        "stream": true,
        "input": input,
        "tools": tools(),
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "reasoning": {"effort": "medium", "summary": "auto"},
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "thread-1",
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn translates_a_tool_turn_and_round_trips_history() {
    let (mock, proxy) = start().await;
    mock.reply(StatusCode::OK, FIRST_TURN);
    mock.reply(StatusCode::OK, SECOND_TURN);

    // Turn 1: thinking + text + tool_use.
    let response = post_responses(proxy, &request_body(base_input())).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let events = parse_sse(&response.text().await.unwrap());
    for (name, data) in &events {
        assert_eq!(Some(name.as_str()), data["type"].as_str());
    }
    let sequence: Vec<String> = events
        .iter()
        .map(|(name, data)| match data["item"]["type"].as_str() {
            Some(item_type) => format!("{name}:{item_type}"),
            None => name.clone(),
        })
        .collect();
    assert_eq!(
        sequence,
        vec![
            "response.created",
            "response.output_item.added:reasoning",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.output_item.done:reasoning",
            "response.output_item.added:message",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_item.done:message",
            "response.output_item.added:function_call",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.output_item.done:function_call",
            "response.completed",
        ]
    );

    let done_items: Vec<Value> = events
        .iter()
        .filter(|(name, _)| name == "response.output_item.done")
        .map(|(_, data)| data["item"].clone())
        .collect();
    assert_eq!(
        done_items[0]["summary"],
        json!([{"type": "summary_text", "text": "The user wants the files listed. I will run ls."}])
    );
    assert_eq!(
        done_items[1],
        json!({
            "type": "message",
            "id": "msg_01Fixture_1",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "Let me list the files.", "annotations": []}],
        })
    );
    assert_eq!(
        done_items[2],
        json!({
            "type": "function_call",
            "id": "fc_01Fixture_2",
            "call_id": "toolu_01Fixture",
            "name": "shell_command",
            "arguments": "{\"workdir\": \"/repo\", \"command\": \"ls -la\"}",
            "status": "completed",
        })
    );
    let completed = &events.last().unwrap().1["response"];
    assert_eq!(completed["id"], json!("resp_01Fixture"));
    assert_eq!(
        completed["usage"],
        json!({
            "input_tokens": 5500,
            "input_tokens_details": {"cached_tokens": 4000, "cache_write_tokens": 300},
            "output_tokens": 87,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 5587,
        })
    );

    // Turn 2: Codex resends the history plus the tool output.
    let mut input = base_input();
    input.extend(done_items);
    input.push(json!({"type": "function_call_output", "call_id": "toolu_01Fixture", "output": "a.txt\nb.txt"}));
    let response = post_responses(proxy, &request_body(input)).await;
    let events = parse_sse(&response.text().await.unwrap());
    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ]
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let (headers, first) = &requests[0];
    assert_eq!(headers.get("x-api-key").unwrap(), "test-key");
    assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(
        first["system"],
        json!([{"type": "text", "text": "You are Codex, a coding agent."}])
    );
    assert_eq!(
        first["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(first["output_config"], json!({"effort": "medium"}));
    assert_eq!(first["stream"], json!(true));

    let (_, second) = &requests[1];
    assert_eq!(
        second["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "List the files in /repo."}]},
            {"role": "assistant", "content": [
                {
                    "type": "thinking",
                    "thinking": "The user wants the files listed. I will run ls.",
                    "signature": SIGNATURE,
                },
                {"type": "text", "text": "Let me list the files."},
                {"type": "tool_use", "id": "toolu_01Fixture", "name": "shell_command", "input": {"workdir": "/repo", "command": "ls -la"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01Fixture", "content": "a.txt\nb.txt"},
            ]},
        ])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_http_errors_keep_codex_semantics() {
    let (mock, proxy) = start().await;
    mock.reply(
        StatusCode::from_u16(529).unwrap(),
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    );
    mock.reply(
        StatusCode::BAD_REQUEST,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.0: unexpected field"}}"#,
    );

    let response = post_responses(proxy, &request_body(base_input())).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], json!("server_is_overloaded"));

    let response = post_responses(proxy, &request_body(base_input())).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body,
        json!({"error": {
            "type": "invalid_request_error",
            "code": null,
            "message": "Anthropic API error (invalid_request_error): messages.0: unexpected field",
        }})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_malformed_requests_without_calling_upstream() {
    let (mock, proxy) = start().await;
    let response = post_responses(proxy, &json!({"input": []})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(mock.requests().is_empty());
}

/// Codex's auth hook; the proxy attaches the Anthropic key itself.
struct NoAuth;

impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) -> Result<(), AuthError> {
        Ok(())
    }
}

/// Codex's own Responses client, configured like the README's custom provider.
fn codex_client(proxy: SocketAddr) -> ResponsesClient<ReqwestTransport> {
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let provider = Provider {
        name: "anthropic-proxy".to_string(),
        base_url: format!("http://{proxy}/v1"),
        query_params: None,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_attempts: 0,
            base_delay: Duration::from_millis(10),
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_secs(10),
    };
    ResponsesClient::new(ReqwestTransport::new(http), provider, Arc::new(NoAuth))
}

fn codex_request(input: Vec<ResponseItem>) -> ResponsesApiRequest {
    let tools: Arc<RawValue> = serde_json::value::to_raw_value(&tools()).unwrap().into();
    ResponsesApiRequest {
        model: "claude-opus-5-5".to_string(),
        stream: true,
        service_tier: None,
        input,
        tools: Some(ResponsesApiTools::from(tools)),
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: Some(Reasoning {
            effort: Some(ReasoningEffort::Medium),
            summary: Some(ReasoningSummary::Auto),
            context: None,
        }),
        store: false,
        stream_options: None,
        include: vec!["reasoning.encrypted_content".to_string()],
        prompt_cache_key: Some("thread-1".to_string()),
        prompt_cache_options: None,
        text: None,
        client_metadata: None,
        access_programs: None,
        request_country: RequestCountry::default(),
    }
}

fn input_message(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn item_kind(item: &ResponseItem) -> &'static str {
    match item {
        ResponseItem::Reasoning { .. } => "reasoning",
        ResponseItem::Message { .. } => "message",
        ResponseItem::FunctionCall { .. } => "function_call",
        ResponseItem::CustomToolCall { .. } => "custom_tool_call",
        _ => "other",
    }
}

/// What Codex's parser made of one proxied turn.
struct CodexTurn {
    labels: Vec<String>,
    done_items: Vec<ResponseItem>,
    usage: Option<codex_protocol::protocol::TokenUsage>,
}

/// Sends `request` with Codex's Responses client and drains the stream its
/// SSE parser produces, failing on any error Codex would surface.
async fn codex_turn(proxy: SocketAddr, request: ResponsesApiRequest) -> CodexTurn {
    let stream = codex_client(proxy)
        .stream_request(request, ResponsesOptions::default())
        .await
        .expect("Codex accepted the proxy's HTTP response");
    let events: Vec<ResponseEvent> = stream
        .map(|event| event.expect("Codex accepted every proxy event"))
        .collect()
        .await;

    let mut turn = CodexTurn {
        labels: Vec::new(),
        done_items: Vec::new(),
        usage: None,
    };
    for event in events {
        let label = match event {
            ResponseEvent::Created { response_id } => {
                format!("created:{}", response_id.unwrap_or_default())
            }
            // Codex reports a snapshot for every HTTP stream; the proxy sends
            // no rate-limit headers, so it must be empty.
            ResponseEvent::RateLimits(snapshot)
                if snapshot.primary.is_none() && snapshot.secondary.is_none() =>
            {
                "rate_limits:none".to_string()
            }
            ResponseEvent::OutputItemAdded(item) => format!("added:{}", item_kind(&item)),
            ResponseEvent::OutputItemDone(item) => {
                let label = format!("done:{}", item_kind(&item));
                turn.done_items.push(item);
                label
            }
            ResponseEvent::OutputTextDelta(delta) => format!("text:{delta}"),
            ResponseEvent::ReasoningSummaryPartAdded { summary_index } => {
                format!("summary_part:{summary_index}")
            }
            ResponseEvent::ReasoningSummaryDelta { delta, .. } => format!("summary:{delta}"),
            ResponseEvent::ReasoningSummaryDone { text, .. } => format!("summary_done:{text}"),
            ResponseEvent::Completed {
                response_id,
                token_usage,
                end_turn,
                ..
            } => {
                turn.usage = token_usage;
                format!("completed:{response_id}:end_turn={end_turn:?}")
            }
            other => format!("other:{other:?}"),
        };
        turn.labels.push(label);
    }
    turn
}

/// Runs the proxy's output through Codex's own client and SSE parser, then
/// replays the parsed items, serialized by Codex's real `ResponseItem` type, as
/// the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_parser_and_history_types_accept_the_translation() {
    let (mock, proxy) = start().await;
    mock.reply(StatusCode::OK, FIRST_TURN);
    mock.reply(StatusCode::OK, SECOND_TURN);

    let base_history = vec![
        input_message("developer", "You are Codex, a coding agent."),
        input_message("user", "List the files in /repo."),
    ];

    // Turn 1: thinking + text + tool_use, as Codex sees it.
    let turn = codex_turn(proxy, codex_request(base_history.clone())).await;
    assert_eq!(
        turn.labels,
        vec![
            "rate_limits:none",
            "created:resp_01Fixture",
            "added:reasoning",
            "summary_part:0",
            "summary:The user wants the files listed.",
            "summary: I will run ls.",
            "summary_done:The user wants the files listed. I will run ls.",
            "done:reasoning",
            "added:message",
            "text:Let me list ",
            "text:the files.",
            "done:message",
            "added:function_call",
            "done:function_call",
            "completed:resp_01Fixture:end_turn=None",
        ]
    );
    let usage = turn.usage.expect("Codex parsed the token usage");
    assert_eq!(
        (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.cache_write_input_tokens,
            usage.output_tokens,
            usage.reasoning_output_tokens,
            usage.total_tokens,
        ),
        (5500, 4000, 300, 87, 0, 5587)
    );

    let [reasoning, message, function_call] = turn.done_items.as_slice() else {
        panic!("expected three output items, got {:?}", turn.done_items);
    };
    let ResponseItem::Reasoning {
        summary,
        encrypted_content,
        ..
    } = reasoning
    else {
        panic!("expected a reasoning item, got {reasoning:?}");
    };
    assert_eq!(
        summary,
        &vec![ReasoningItemReasoningSummary::SummaryText {
            text: "The user wants the files listed. I will run ls.".to_string(),
        }]
    );
    assert!(
        encrypted_content
            .as_deref()
            .is_some_and(|content| content.starts_with("anthropic-thinking-v1:")),
        "reasoning must carry the signed thinking block: {encrypted_content:?}"
    );
    let ResponseItem::Message { role, content, .. } = message else {
        panic!("expected an assistant message, got {message:?}");
    };
    assert_eq!(role, "assistant");
    assert!(
        matches!(content.as_slice(), [ContentItem::OutputText { text, .. }] if text == "Let me list the files."),
        "unexpected message content: {content:?}"
    );
    let ResponseItem::FunctionCall {
        name,
        namespace,
        call_id,
        arguments,
        ..
    } = function_call
    else {
        panic!("expected a function call, got {function_call:?}");
    };
    assert_eq!(
        (
            name.as_str(),
            namespace.as_deref(),
            call_id.as_str(),
            arguments.as_str()
        ),
        (
            "shell_command",
            None,
            "toolu_01Fixture",
            "{\"workdir\": \"/repo\", \"command\": \"ls -la\"}",
        )
    );

    // Turn 2: Codex's history (its own items plus the tool output), encoded by
    // Codex's request type.
    let mut history = base_history;
    history.extend(turn.done_items);
    history.push(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some("toolu_01Fixture".to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text("a.txt\nb.txt".to_string()),
        internal_chat_message_metadata_passthrough: None,
    });
    let turn = codex_turn(proxy, codex_request(history)).await;
    assert_eq!(
        turn.labels,
        vec![
            "rate_limits:none",
            "created:resp_02Fixture",
            "added:message",
            "text:There are two files.",
            "done:message",
            "completed:resp_02Fixture:end_turn=None",
        ]
    );

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let (_, second) = &requests[1];
    assert_eq!(
        second["system"],
        json!([{"type": "text", "text": "You are Codex, a coding agent."}])
    );
    assert_eq!(
        second["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "List the files in /repo."}]},
            {"role": "assistant", "content": [
                {
                    "type": "thinking",
                    "thinking": "The user wants the files listed. I will run ls.",
                    "signature": SIGNATURE,
                },
                {"type": "text", "text": "Let me list the files."},
                {"type": "tool_use", "id": "toolu_01Fixture", "name": "shell_command", "input": {"workdir": "/repo", "command": "ls -la"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01Fixture", "content": "a.txt\nb.txt"},
            ]},
        ])
    );
}
