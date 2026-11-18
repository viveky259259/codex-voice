//! Optional recording of translated exchanges, for debugging and fixtures.
//!
//! Each exchange writes `<seq>-<ts>-request.json` (the Responses request and
//! the translated Anthropic request), then either `-anthropic.sse` and
//! `-responses.sse` (the raw streams) or `-anthropic-error.json`. Headers are
//! never recorded, so the API key cannot leak into dumps.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use serde_json::json;
use tracing::warn;

#[derive(Debug)]
pub(crate) struct Dumper {
    dump_dir: PathBuf,
    next_sequence: AtomicU64,
}

impl Dumper {
    pub(crate) fn new(dump_dir: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&dump_dir)?;
        Ok(Self {
            dump_dir,
            next_sequence: AtomicU64::new(1),
        })
    }

    pub(crate) async fn begin(
        &self,
        responses_request: &Value,
        anthropic_request: &[u8],
    ) -> Option<ExchangeDump> {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let prefix = format!("{sequence:06}-{timestamp_ms}");
        let anthropic_request = serde_json::from_slice::<Value>(anthropic_request)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(anthropic_request).into()));
        let request = json!({
            "responses_request": responses_request,
            "anthropic_request": anthropic_request,
        });
        let path = self.dump_dir.join(format!("{prefix}-request.json"));
        let contents = serde_json::to_vec_pretty(&request).unwrap_or_default();
        if let Err(err) = tokio::fs::write(&path, contents).await {
            warn!("failed to write {}: {err}", path.display());
            return None;
        }
        Some(ExchangeDump {
            dump_dir: self.dump_dir.clone(),
            prefix,
            anthropic_stream: String::new(),
            responses_stream: String::new(),
        })
    }
}

#[derive(Debug)]
pub(crate) struct ExchangeDump {
    dump_dir: PathBuf,
    prefix: String,
    anthropic_stream: String,
    responses_stream: String,
}

impl ExchangeDump {
    pub(crate) fn record_anthropic_event(&mut self, event: &str, data: &str) {
        if !event.is_empty() {
            self.anthropic_stream.push_str(&format!("event: {event}\n"));
        }
        self.anthropic_stream.push_str(&format!("data: {data}\n\n"));
    }

    pub(crate) fn record_responses_bytes(&mut self, bytes: &[u8]) {
        self.responses_stream
            .push_str(&String::from_utf8_lossy(bytes));
    }

    pub(crate) async fn finish_error(self, status: u16, body: &str) {
        let body = serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!(body));
        let contents =
            serde_json::to_vec_pretty(&json!({"status": status, "body": body})).unwrap_or_default();
        self.write("anthropic-error.json", contents).await;
    }

    pub(crate) async fn finish_stream(mut self) {
        let anthropic = std::mem::take(&mut self.anthropic_stream).into_bytes();
        let responses = std::mem::take(&mut self.responses_stream).into_bytes();
        self.write("anthropic.sse", anthropic).await;
        self.write("responses.sse", responses).await;
    }

    async fn write(&self, suffix: &str, contents: Vec<u8>) {
        let path = self.dump_dir.join(format!("{}-{suffix}", self.prefix));
        if let Err(err) = tokio::fs::write(&path, contents).await {
            warn!("failed to write {}: {err}", path.display());
        }
    }
}
