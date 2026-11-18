use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::path_str;
use crate::test_support::pcm_bytes;
use crate::test_support::speech_burst;
use crate::test_support::write_script;

#[tokio::test]
async fn reads_pcm_from_a_fake_command() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("mic.raw");
    let samples = speech_burst();
    let mut bytes = pcm_bytes(&samples);
    // A trailing odd byte must not produce a sample.
    bytes.push(0x7f);
    std::fs::write(&fixture, &bytes).unwrap();

    let config = CaptureConfig {
        enabled: true,
        command: vec!["cat".to_string(), "{device}".to_string()],
        device: path_str(&fixture),
    };
    let mut capture = Capture::spawn(&config).unwrap();
    let mut got = Vec::new();
    while let Some(block) = capture.next_samples().await {
        got.extend(block);
    }
    assert_eq!(got, samples);
    let message = capture.exit_message().await;
    assert!(message.contains("`cat` exited"), "{message}");
}

#[tokio::test]
async fn dead_command_reports_stderr_tail() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "fake-ffmpeg",
        "echo 'Input/output error: no such audio device' >&2\nexit 1",
    );
    let config = CaptureConfig {
        enabled: true,
        command: vec![path_str(&script)],
        device: String::new(),
    };
    let mut capture = Capture::spawn(&config).unwrap();
    assert_eq!(capture.next_samples().await, None);
    let message = capture.exit_message().await;
    assert!(message.contains("no such audio device"), "{message}");
    assert!(message.contains("exit status: 1"), "{message}");
}

#[tokio::test]
async fn missing_program_is_an_error_not_a_panic() {
    let config = CaptureConfig {
        enabled: true,
        command: vec!["/nonexistent/ffmpeg".to_string()],
        device: String::new(),
    };
    let err = Capture::spawn(&config).err().unwrap();
    assert!(err.to_string().contains("/nonexistent/ffmpeg"), "{err}");
}
