use std::sync::Arc;
use std::sync::Mutex;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::routing::get;
use axum::routing::post;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;
use crate::config::McpJobConfig;
use crate::mcp::tests::job_handler;
use crate::mcp::tests::start_mock_mcp;
use crate::test_support::path_str;

#[test]
fn prepare_text_trims_and_caps() {
    assert_eq!(prepare_text("   ", 400), None);
    assert_eq!(prepare_text("- - ", 400), None);
    assert_eq!(
        prepare_text("--rate 10\n  hello", 400),
        Some("rate 10 hello".to_string())
    );
    assert_eq!(
        prepare_text("one two three four", 9),
        Some("one two".to_string())
    );
    let long = "word ".repeat(200);
    let capped = prepare_text(&long, 400).unwrap();
    assert!(capped.chars().count() <= 400);
    assert!(capped.ends_with("word"));
    // Multi-byte text is cut on a char boundary.
    assert_eq!(prepare_text("ééééé", 3), Some("ééé".to_string()));
}

#[tokio::test]
async fn openai_compatible_posts_speech_request_and_writes_wav() {
    let received = Arc::new(Mutex::new(None));
    async fn speech(
        State(received): State<Arc<Mutex<Option<serde_json::Value>>>>,
        body: Bytes,
    ) -> Vec<u8> {
        *received.lock().unwrap() = Some(serde_json::from_slice(&body).unwrap());
        b"RIFF-kokoro".to_vec()
    }
    let app = Router::new()
        .route("/v1/audio/speech", post(speech))
        .with_state(Arc::clone(&received));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("speech.wav");
    let tts = OpenAiTts::new(OpenAiTtsConfig {
        base_url,
        ..OpenAiTtsConfig::default()
    })
    .unwrap();
    let speech = tts.synthesize("Tests pass.", &out).await.unwrap();
    server.abort();
    assert_eq!(
        speech,
        Speech::Wav {
            path: out.clone(),
            delete_after: true
        }
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"RIFF-kokoro");
    assert_eq!(
        received.lock().unwrap().clone().unwrap(),
        json!({
            "model": "kokoro",
            "input": "Tests pass.",
            "voice": "af_heart",
            "response_format": "wav",
        })
    );
}

#[tokio::test]
async fn mcp_backend_plays_the_job_artifact() {
    let artifacts = tempfile::tempdir().unwrap();
    let artifact = artifacts.path().join("job-7").join("speech.wav");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, b"RIFF").unwrap();
    let mock = start_mock_mcp(
        &["tts", "get_job"],
        true,
        job_handler(
            "tts",
            json!({ "artifacts": [{ "name": "speech.wav", "path": path_str(&artifact) }] }),
        ),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.wav");
    let tts = McpTts::new(McpTtsConfig {
        url: mock.url.clone(),
        tool: "tts".to_string(),
        job: McpJobConfig {
            poll_interval_ms: 10,
            ..McpJobConfig::default()
        },
        ..McpTtsConfig::default()
    })
    .unwrap();
    tts.prepare().await.unwrap();
    let speech = tts.synthesize("Say \"hi\"; $(x)", &out).await.unwrap();
    assert_eq!(
        speech,
        Speech::Wav {
            path: artifact.clone(),
            delete_after: false
        }
    );
    let calls = mock.tool_calls();
    assert_eq!(
        calls[0],
        (
            "tts".to_string(),
            json!({
                "model_id": "hf:mlx-community/Kokoro-82M-8bit",
                "voice": "af_heart",
                "text": "Say \"hi\"; $(x)",
            })
        )
    );
}

#[tokio::test]
async fn mcp_backend_downloads_artifact_urls_and_resolves_relative_paths() {
    async fn wav() -> Vec<u8> {
        b"RIFF-url".to_vec()
    }
    let app = Router::new().route("/artifacts/speech.wav", get(wav));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let artifact_url = format!(
        "http://{}/artifacts/speech.wav",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // Synchronous tool: the result already holds the audio location.
    let url = artifact_url.clone();
    let mock = start_mock_mcp(
        &["speak", "get_job"],
        false,
        Arc::new(move |_, _| json!({ "id": "a1", "audio_url": url })),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.wav");
    let tts = McpTts::new(McpTtsConfig {
        url: mock.url.clone(),
        audio_path: Some("audio_url".to_string()),
        ..McpTtsConfig::default()
    })
    .unwrap();
    let speech = tts.synthesize("hello", &out).await.unwrap();
    server.abort();
    assert_eq!(
        speech,
        Speech::Wav {
            path: out.clone(),
            delete_after: true
        }
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"RIFF-url");
    assert_eq!(mock.tool_calls().len(), 1);

    let artifacts = tempfile::tempdir().unwrap();
    std::fs::write(artifacts.path().join("speech.wav"), b"RIFF").unwrap();
    let mock = start_mock_mcp(
        &["speak", "get_job"],
        false,
        Arc::new(|_, _| json!({ "status": "done", "job_id": "j", "output": "speech.wav" })),
    )
    .await;
    let tts = McpTts::new(McpTtsConfig {
        url: mock.url.clone(),
        artifact_dir: Some(path_str(artifacts.path())),
        ..McpTtsConfig::default()
    })
    .unwrap();
    let speech = tts.synthesize("hello", &out).await.unwrap();
    assert_eq!(
        speech,
        Speech::Wav {
            path: artifacts.path().join("speech.wav"),
            delete_after: false
        }
    );
}
