//! End-to-end tests: Codex-shaped Responses requests go through the proxy to a
//! mock Anthropic server that replays canned Messages streams.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

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
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
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

fn request_body(input: Vec<Value>) -> Value {
    json!({
        "model": "claude-opus-5-5",
        "stream": true,
        "input": input,
        "tools": [{
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
        }],
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
