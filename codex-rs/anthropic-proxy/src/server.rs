//! HTTP front end: accepts Responses requests and streams translated events.

use std::convert::Infallible;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::RETRY_AFTER;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::Value;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::dump::Dumper;
use crate::dump::ExchangeDump;
use crate::request::TranslateOptions;
use crate::request::TranslatedRequest;
use crate::request::translate_request;
use crate::stream::StreamTranslator;
use crate::stream::encode_sse;
use crate::stream::is_context_overflow;

pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic API key. `Debug` never prints the value.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: String) -> Self {
        Self(key)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey([REDACTED])")
    }
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Base URL; requests go to `{anthropic_base_url}/v1/messages`.
    pub anthropic_base_url: String,
    pub api_key: ApiKey,
    pub anthropic_version: String,
    /// Optional `anthropic-beta` header value.
    pub anthropic_beta: Option<String>,
    pub translate: TranslateOptions,
    /// Directory where request/stream pairs are recorded.
    pub dump_dir: Option<PathBuf>,
}

impl ProxyConfig {
    pub fn new(api_key: ApiKey) -> Self {
        Self {
            anthropic_base_url: DEFAULT_ANTHROPIC_BASE_URL.to_string(),
            api_key,
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.to_string(),
            anthropic_beta: None,
            translate: TranslateOptions::default(),
            dump_dir: None,
        }
    }
}

struct AppState {
    config: ProxyConfig,
    messages_url: String,
    client: reqwest::Client,
    dumper: Option<Dumper>,
}

/// Builds the proxy router: `POST /v1/responses` (also `/responses`) and `GET /health`.
pub fn router(config: ProxyConfig) -> anyhow::Result<Router> {
    let messages_url = format!(
        "{}/v1/messages",
        config.anthropic_base_url.trim_end_matches('/')
    );
    let upstream = reqwest::Url::parse(&messages_url)?;
    let mut client = reqwest::Client::builder()
        .user_agent(concat!("codex-anthropic-proxy/", env!("CARGO_PKG_VERSION")));
    if matches!(
        upstream.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    ) {
        client = client.no_proxy();
    }
    let dumper = config.dump_dir.clone().map(Dumper::new).transpose()?;
    let state = Arc::new(AppState {
        config,
        messages_url,
        client: client.build()?,
        dumper,
    });
    Ok(Router::new()
        .route("/v1/responses", post(handle_responses))
        .route("/responses", post(handle_responses))
        .route("/health", get(|| async { "ok" }))
        // Codex sends the full history on every request.
        .layer(DefaultBodyLimit::disable())
        .with_state(state))
}

/// Serves the proxy on an already-bound listener until the process exits.
pub async fn serve(listener: TcpListener, config: ProxyConfig) -> anyhow::Result<()> {
    let app = router(config)?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_responses(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                None,
                &format!("request body is not valid JSON: {err}"),
            );
        }
    };
    let TranslatedRequest {
        body: anthropic_request,
        tools,
    } = match translate_request(&request, &state.config.translate) {
        Ok(translated) => translated,
        Err(err) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                None,
                &err.to_string(),
            );
        }
    };
    let model = anthropic_request.model.clone();
    let anthropic_body = match serde_json::to_vec(&anthropic_request) {
        Ok(body) => body,
        Err(err) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                None,
                &format!("failed to encode Anthropic request: {err}"),
            );
        }
    };
    let dump = match &state.dumper {
        Some(dumper) => dumper.begin(&request, &anthropic_body).await,
        None => None,
    };
    debug!(
        model = %model,
        messages = anthropic_request.messages.len(),
        tools = anthropic_request.tools.len(),
        "forwarding request to Anthropic"
    );

    let mut upstream = state
        .client
        .post(&state.messages_url)
        .header("x-api-key", &state.config.api_key.0)
        .header("anthropic-version", &state.config.anthropic_version)
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "text/event-stream");
    if let Some(beta) = &state.config.anthropic_beta {
        upstream = upstream.header("anthropic-beta", beta);
    }
    let response = match upstream.body(anthropic_body).send().await {
        Ok(response) => response,
        Err(err) => {
            warn!("Anthropic request failed: {err}");
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "server_error",
                None,
                &format!("failed to reach the Anthropic API: {err}"),
            );
        }
    };

    let status = response.status();
    if !status.is_success() {
        let retry_after = response.headers().get(RETRY_AFTER).cloned();
        let text = response.text().await.unwrap_or_default();
        info!(status = status.as_u16(), "Anthropic returned an error");
        if let Some(dump) = dump {
            dump.finish_error(status.as_u16(), &text).await;
        }
        return upstream_error(status, retry_after, &text);
    }

    let (tx, mut rx) = mpsc::channel::<Bytes>(64);
    tokio::spawn(pump_stream(
        response,
        StreamTranslator::new(tools, model),
        tx,
        dump,
    ));
    let body = futures::stream::poll_fn(move |cx| {
        rx.poll_recv(cx)
            .map(|chunk| chunk.map(Ok::<Bytes, Infallible>))
    });
    let mut response = Body::from_stream(body).into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

async fn pump_stream(
    response: reqwest::Response,
    mut translator: StreamTranslator,
    tx: mpsc::Sender<Bytes>,
    mut dump: Option<ExchangeDump>,
) {
    let mut events = response.bytes_stream().eventsource();
    let mut client_connected = true;
    while let Some(event) = events.next().await {
        let translated = match event {
            Ok(event) => {
                if let Some(dump) = dump.as_mut() {
                    dump.record_anthropic_event(&event.event, &event.data);
                }
                if event.data.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&event.data) {
                    Ok(event) => translator.on_event(&event),
                    Err(err) => {
                        debug!("skipping undecodable Anthropic event: {err}");
                        continue;
                    }
                }
            }
            Err(err) => translator.fail(
                "server_error",
                &format!("error reading Anthropic stream: {err}"),
            ),
        };
        client_connected = send_events(&tx, translated, dump.as_mut()).await;
        if !client_connected || translator.is_finished() {
            break;
        }
    }
    if client_connected {
        let trailing = translator.finish();
        send_events(&tx, trailing, dump.as_mut()).await;
    } else {
        // Dropping the upstream stream cancels the Anthropic request.
        debug!("Codex disconnected; abandoning the Anthropic stream");
    }
    if let Some(dump) = dump {
        dump.finish_stream().await;
    }
}

async fn send_events(
    tx: &mpsc::Sender<Bytes>,
    events: Vec<Value>,
    mut dump: Option<&mut ExchangeDump>,
) -> bool {
    for event in events {
        let frame = encode_sse(&event);
        if let Some(dump) = dump.as_deref_mut() {
            dump.record_responses_bytes(&frame);
        }
        if tx.send(frame).await.is_err() {
            return false;
        }
    }
    true
}

/// Maps a non-2xx Anthropic response onto the HTTP error shape Codex's
/// Responses client understands, keeping retry semantics.
fn upstream_error(status: StatusCode, retry_after: Option<HeaderValue>, body: &str) -> Response {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let error = parsed.as_ref().and_then(|parsed| parsed.get("error"));
    let error_type = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            if body.trim().is_empty() {
                status.canonical_reason().unwrap_or("error").to_string()
            } else {
                body.to_string()
            }
        });
    let (status, code) = match status.as_u16() {
        400 | 413 if is_context_overflow(&message) => {
            (StatusCode::BAD_REQUEST, Some("context_length_exceeded"))
        }
        413 => (StatusCode::BAD_REQUEST, None),
        429 => (StatusCode::TOO_MANY_REQUESTS, Some("rate_limit_exceeded")),
        // 529 is Anthropic's "overloaded" status.
        529 => (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("server_is_overloaded"),
        ),
        _ => (status, None),
    };
    let mut response = openai_error(
        status,
        error_type,
        code,
        &format!("Anthropic API error ({error_type}): {message}"),
    );
    if let Some(retry_after) = retry_after {
        response.headers_mut().insert(RETRY_AFTER, retry_after);
    }
    response
}

fn openai_error(
    status: StatusCode,
    error_type: &str,
    code: Option<&str>,
    message: &str,
) -> Response {
    let body = json!({
        "error": {
            "type": error_type,
            "code": code,
            "message": message,
        },
    });
    let mut response = (status, body.to_string()).into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    async fn body_json(response: Response) -> (StatusCode, Option<HeaderValue>, Value) {
        let status = response.status();
        let retry_after = response.headers().get(RETRY_AFTER).cloned();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        (
            status,
            retry_after,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn maps_context_overflow_to_codex_code() {
        let response = upstream_error(
            StatusCode::BAD_REQUEST,
            None,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
        );
        let (status, _, body) = body_json(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({"error": {
                "type": "invalid_request_error",
                "code": "context_length_exceeded",
                "message": "Anthropic API error (invalid_request_error): prompt is too long: 250000 tokens > 200000 maximum",
            }})
        );
    }

    #[tokio::test]
    async fn maps_overloaded_and_keeps_retry_after() {
        let response = upstream_error(
            StatusCode::from_u16(529).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            Some(HeaderValue::from_static("7")),
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        let (status, retry_after, body) = body_json(response).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(retry_after, Some(HeaderValue::from_static("7")));
        assert_eq!(body["error"]["code"], json!("server_is_overloaded"));
    }
}
