use std::sync::Arc;
use std::sync::Mutex;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::config::McpJobConfig;
use crate::mcp::tests::job_handler;
use crate::mcp::tests::start_mock_mcp;
use crate::test_support::path_str;
use crate::test_support::read_argv;
use crate::test_support::write_script;
use crate::wav::encode_wav;

fn fixture_wav(dir: &Path) -> std::path::PathBuf {
    let wav = dir.join("utterance one.wav");
    std::fs::write(&wav, encode_wav(&[1, 2, 3, 4], 16_000)).unwrap();
    wav
}

#[tokio::test]
async fn command_backend_reads_stdout_and_passes_the_wav_as_one_argument() {
    let dir = tempfile::tempdir().unwrap();
    let argv_file = dir.path().join("argv");
    let script = write_script(
        dir.path(),
        "fake-whisper",
        &format!(
            "printf '%s\\0' \"$@\" > '{}'\necho '  hello from whisper  '",
            argv_file.display()
        ),
    );
    let wav = fixture_wav(dir.path());
    let scratch = tempfile::tempdir().unwrap();
    let stt = CommandStt::new(CommandSttConfig {
        command: vec![
            path_str(&script),
            "-f".to_string(),
            "{wav}".to_string(),
            "-nt".to_string(),
        ],
        timeout_ms: 10_000,
    })
    .unwrap();
    let text = stt.transcribe(&wav, scratch.path()).await.unwrap();
    assert_eq!(text.trim(), "hello from whisper");
    assert_eq!(
        read_argv(&argv_file),
        vec!["-f".to_string(), path_str(&wav), "-nt".to_string()]
    );
}

#[tokio::test]
async fn command_backend_reads_out_file_and_outdir() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture_wav(dir.path());

    // {out}: the command writes the transcript to the given file.
    let script = write_script(dir.path(), "to-file", "echo 'from out file' > \"$2\"");
    let scratch = tempfile::tempdir().unwrap();
    let stt = CommandStt::new(CommandSttConfig {
        command: vec![path_str(&script), "{wav}".to_string(), "{out}".to_string()],
        timeout_ms: 10_000,
    })
    .unwrap();
    assert_eq!(
        stt.transcribe(&wav, scratch.path()).await.unwrap().trim(),
        "from out file"
    );

    // {outdir}: like mlx_whisper --output-format txt --output-dir DIR.
    let script = write_script(
        dir.path(),
        "fake-mlx-whisper",
        "echo 'progress noise'\nbase=$(basename \"$1\" .wav)\necho 'from outdir' > \"$3/$base.txt\"",
    );
    let scratch = tempfile::tempdir().unwrap();
    let stt = CommandStt::new(CommandSttConfig {
        command: vec![
            path_str(&script),
            "{wav}".to_string(),
            "--output-dir".to_string(),
            "{outdir}".to_string(),
        ],
        timeout_ms: 10_000,
    })
    .unwrap();
    assert_eq!(
        stt.transcribe(&wav, scratch.path()).await.unwrap().trim(),
        "from outdir"
    );
}

#[tokio::test]
async fn command_backend_failure_carries_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture_wav(dir.path());
    let script = write_script(dir.path(), "broken", "echo 'model not found' >&2\nexit 3");
    let stt = CommandStt::new(CommandSttConfig {
        command: vec![path_str(&script), "{wav}".to_string()],
        timeout_ms: 10_000,
    })
    .unwrap();
    let err = stt
        .transcribe(&wav, dir.path())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("model not found"), "{err}");
}

#[derive(Default)]
struct Recorded {
    content_type: String,
    authorization: String,
    body: Vec<u8>,
}

#[tokio::test]
async fn openai_compatible_posts_multipart() {
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    async fn handler(
        State(recorded): State<Arc<Mutex<Recorded>>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> ([(&'static str, &'static str); 1], String) {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        *recorded.lock().unwrap() = Recorded {
            content_type: header("content-type"),
            authorization: header("authorization"),
            body: body.to_vec(),
        };
        (
            [("content-type", "application/json")],
            json!({ "text": " transcribed words " }).to_string(),
        )
    }
    let app = Router::new()
        .route("/v1/audio/transcriptions", post(handler))
        .with_state(Arc::clone(&recorded));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let wav = fixture_wav(dir.path());
    // SAFETY: a variable name that only this test reads.
    unsafe { std::env::set_var("CODEX_LOCAL_VOICE_TEST_KEY", "sk-local") };
    let stt = OpenAiStt::new(OpenAiSttConfig {
        base_url,
        model: "base.en".to_string(),
        language: Some("en".to_string()),
        api_key_env: Some("CODEX_LOCAL_VOICE_TEST_KEY".to_string()),
        ..OpenAiSttConfig::default()
    })
    .unwrap();
    let text = stt.transcribe(&wav, dir.path()).await.unwrap();
    server.abort();
    assert_eq!(text, " transcribed words ");

    let recorded = recorded.lock().unwrap();
    assert!(
        recorded
            .content_type
            .starts_with("multipart/form-data; boundary="),
        "{}",
        recorded.content_type
    );
    assert_eq!(recorded.authorization, "Bearer sk-local");
    let body = String::from_utf8_lossy(&recorded.body);
    assert!(body.contains("name=\"model\"\r\n\r\nbase.en\r\n"), "{body}");
    assert!(body.contains("name=\"language\"\r\n\r\nen\r\n"), "{body}");
    assert!(
        body.contains("name=\"file\"; filename=\"utterance.wav\""),
        "{body}"
    );
    let wav_bytes = std::fs::read(&wav).unwrap();
    assert!(
        recorded
            .body
            .windows(wav_bytes.len())
            .any(|window| window == wav_bytes.as_slice())
    );
}

#[tokio::test]
async fn mcp_backend_runs_a_transcription_job() {
    let mock = start_mock_mcp(
        &["transcribe_audio", "get_job", "speak"],
        false,
        job_handler("transcribe_audio", json!({ "text": "list my agents" })),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let wav = fixture_wav(dir.path());
    let stt = McpStt::new(McpSttConfig {
        url: mock.url.clone(),
        tool: "transcribe_audio".to_string(),
        job: McpJobConfig {
            poll_interval_ms: 10,
            ..McpJobConfig::default()
        },
        ..McpSttConfig::default()
    })
    .unwrap();
    stt.prepare().await.unwrap();
    let text = stt.transcribe(&wav, dir.path()).await.unwrap();
    assert_eq!(text, "list my agents");
    let calls = mock.tool_calls();
    assert_eq!(
        calls[0],
        (
            "transcribe_audio".to_string(),
            json!({ "model_id": "whisper:base", "input_path": path_str(&wav) })
        )
    );
    assert_eq!(calls.last().unwrap().0, "get_job");
}

#[tokio::test]
async fn mcp_backend_names_available_tools_when_misconfigured() {
    let mock = start_mock_mcp(&["run_model", "get_job"], false, Arc::new(|_, _| json!({}))).await;
    let stt = McpStt::new(McpSttConfig {
        url: mock.url.clone(),
        ..McpSttConfig::default()
    })
    .unwrap();
    let err = stt.prepare().await.unwrap_err().to_string();
    assert!(err.contains("no tool `transcribe`"), "{err}");
    assert!(err.contains("run_model, get_job"), "{err}");
}

#[test]
fn endpoint_joins_without_doubling_v1() {
    assert_eq!(
        endpoint("http://h:1/v1/", "/v1/audio/speech"),
        "http://h:1/v1/audio/speech"
    );
    assert_eq!(endpoint("http://h:1", "/inference"), "http://h:1/inference");
}
