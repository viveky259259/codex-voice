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
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use super::*;

pub(crate) type ToolHandler = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

/// A fake MCP server over streamable HTTP. Tool handlers return the payload,
/// which is sent back as a JSON text block.
pub(crate) struct MockMcp {
    pub url: String,
    pub calls: Arc<Mutex<Vec<(String, Value)>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for MockMcp {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl MockMcp {
    pub(crate) fn tool_calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

struct MockState {
    tools: Vec<String>,
    sse: bool,
    handler: ToolHandler,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

pub(crate) async fn start_mock_mcp(tools: &[&str], sse: bool, handler: ToolHandler) -> MockMcp {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(MockState {
        tools: tools.iter().map(|tool| (*tool).to_string()).collect(),
        sse,
        handler,
        calls: Arc::clone(&calls),
    });
    let app = Router::new()
        .route("/mcp", post(mock_mcp))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    MockMcp { url, calls, server }
}

async fn mock_mcp(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let message: Value = serde_json::from_slice(&body).unwrap();
    let method = message["method"].as_str().unwrap_or_default().to_string();
    let Some(id) = message.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    if method == "initialize" {
        let reply = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mock", "version": "0" },
            },
        });
        return (
            [
                ("mcp-session-id", "session-1"),
                ("content-type", "application/json"),
            ],
            reply.to_string(),
        )
            .into_response();
    }
    if headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        != Some("session-1")
    {
        return (StatusCode::BAD_REQUEST, "missing session").into_response();
    }
    let result = match method.as_str() {
        "tools/list" => json!({
            "tools": state.tools.iter().map(|name| json!({
                "name": name,
                "description": format!("{name} tool"),
                "inputSchema": { "type": "object", "properties": {} },
            })).collect::<Vec<_>>(),
        }),
        "tools/call" => {
            let name = message["params"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let arguments = message["params"]["arguments"].clone();
            state
                .calls
                .lock()
                .unwrap()
                .push((name.clone(), arguments.clone()));
            let payload = (state.handler)(&name, &arguments);
            json!({ "content": [{ "type": "text", "text": payload.to_string() }], "isError": false })
        }
        other => panic!("unexpected method {other}"),
    };
    let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    if state.sse {
        let body = format!(
            "event: message\ndata: {}\n\n",
            json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}})
        ) + &format!("event: message\ndata: {reply}\n\n");
        ([("content-type", "text/event-stream")], body).into_response()
    } else {
        ([("content-type", "application/json")], reply.to_string()).into_response()
    }
}

/// A job tool: the first call returns a job id, `get_job` is pending for
/// two polls, then done with `result`.
pub(crate) fn job_handler(start_tool: &'static str, result: Value) -> ToolHandler {
    let polls = Arc::new(Mutex::new(0));
    Arc::new(move |name, _arguments| {
        if name == start_tool {
            return json!({ "job_id": "job-7", "status": "queued" });
        }
        assert_eq!(name, "get_job");
        let mut polls = polls.lock().unwrap();
        *polls += 1;
        if *polls < 3 {
            json!({ "job_id": "job-7", "status": "running" })
        } else {
            json!({ "job_id": "job-7", "status": "done", "result": result.clone() })
        }
    })
}

#[tokio::test]
async fn lists_tools_and_names_them_when_one_is_missing() {
    let mock = start_mock_mcp(&["submit", "get_job"], false, Arc::new(|_, _| json!({}))).await;
    let client = McpClient::new(&mock.url).unwrap();
    let names: Vec<String> = client
        .tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(names, vec!["submit".to_string(), "get_job".to_string()]);
    client
        .require_tools(&[("stt.tool", "submit"), ("stt.job.tool", "get_job")])
        .await
        .unwrap();
    let err = client
        .require_tools(&[("stt.tool", "transcribe")])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no tool `transcribe`"), "{err}");
    assert!(err.contains("available tools: submit, get_job"), "{err}");
}

#[tokio::test]
async fn polls_a_job_until_done_over_sse() {
    let mock = start_mock_mcp(
        &["transcribe", "get_job"],
        true,
        job_handler("transcribe", json!({ "text": "hello" })),
    )
    .await;
    let client = McpClient::new(&mock.url).unwrap();
    let job = McpJobConfig {
        poll_interval_ms: 10,
        ..McpJobConfig::default()
    };
    let done = client
        .run_job(
            "transcribe",
            json!({ "input_path": "/x.wav" }),
            &job,
            |_| false,
        )
        .await
        .unwrap();
    assert_eq!(json_path(&done, "result.text"), Some(&json!("hello")));
    let calls = mock.tool_calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls[0],
        ("transcribe".to_string(), json!({ "input_path": "/x.wav" }))
    );
    assert_eq!(
        calls[1],
        ("get_job".to_string(), json!({ "job_id": "job-7" }))
    );
}

#[tokio::test]
async fn failed_job_is_an_error() {
    let handler: ToolHandler = Arc::new(|name, _| {
        if name == "speak" {
            json!({ "job_id": 12 })
        } else {
            json!({ "status": "failed", "error": "model not loaded" })
        }
    });
    let mock = start_mock_mcp(&["speak", "get_job"], false, handler).await;
    let client = McpClient::new(&mock.url).unwrap();
    let job = McpJobConfig {
        poll_interval_ms: 10,
        ..McpJobConfig::default()
    };
    let err = client
        .run_job("speak", json!({}), &job, |_| false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("job 12 failed: model not loaded"), "{err}");
}

#[test]
fn json_paths_and_fallbacks() {
    let value = json!({ "result": { "artifacts": [{ "path": "/a/speech.wav" }] }, "n": 3 });
    assert_eq!(
        json_path(&value, "result.artifacts.0.path"),
        Some(&json!("/a/speech.wav"))
    );
    assert_eq!(
        find_string(&value, Some("missing"), &["n", "result"]),
        Some("3".to_string())
    );
    assert_eq!(find_string(&value, None, &["result"]), None);
}
