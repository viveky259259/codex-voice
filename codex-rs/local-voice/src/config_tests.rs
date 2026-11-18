use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

const README: &str = include_str!("../README.md");

/// The ```toml blocks of the README, in order.
fn readme_toml_blocks() -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in README.lines() {
        match (&mut current, line.trim()) {
            (None, "```toml") => current = Some(String::new()),
            (Some(block), "```") => {
                blocks.push(std::mem::take(block));
                current = None;
            }
            (Some(block), _) => {
                block.push_str(line);
                block.push('\n');
            }
            (None, _) => {}
        }
    }
    blocks
}

#[test]
fn empty_config_uses_os_defaults() {
    let config = LocalVoiceConfig::from_toml_str("").unwrap();
    assert_eq!(config, LocalVoiceConfig::default());
    assert!(config.capture.enabled);
    assert_eq!(config.capture.command[0], "ffmpeg");
    assert!(config.capture.command.contains(&"s16le".to_string()));
    assert_eq!(config.playback.max_chars, 400);
    assert_eq!(config.vad.end_silence_ms, 700);
    assert_eq!(config.vad.pre_roll_ms, 300);
    assert_eq!(config.playback.echo_tail_ms, 400);
    assert!(!config.playback.barge_in);
}

#[test]
fn readme_examples_parse() {
    let blocks = readme_toml_blocks();
    assert!(blocks.len() >= 3, "found {} toml blocks", blocks.len());
    for block in &blocks {
        if let Err(err) = LocalVoiceConfig::from_toml_str(block) {
            panic!("README example does not parse: {err}\n{block}");
        }
    }
}

#[test]
fn mcp_example_has_the_expected_shape() {
    let config = LocalVoiceConfig::from_toml_str(
        r#"
[stt]
backend = "mcp"
url = "http://127.0.0.1:5051/mcp"
tool = "transcribe"
arguments = { model_id = "whisper:base", input_path = "{wav}" }
text_path = "result.text"

[stt.job]
tool = "get_job"
poll_interval_ms = 300

[tts]
backend = "mcp"
tool = "speak"
arguments = { model_id = "hf:mlx-community/Kokoro-82M-8bit", voice = "af_heart", text = "{text}" }

[transcripts]
drop = ["[BLANK_AUDIO]"]
"#,
    )
    .unwrap();
    let SttConfig::Mcp(stt) = config.stt else {
        panic!("expected mcp stt");
    };
    assert_eq!(stt.text_path.as_deref(), Some("result.text"));
    assert_eq!(
        stt.arguments,
        json!({ "model_id": "whisper:base", "input_path": "{wav}" })
    );
    assert_eq!(stt.job.tool, "get_job");
    assert_eq!(stt.job.poll_interval_ms, 300);
    assert_eq!(stt.job.arguments, json!({ "job_id": "{job_id}" }));
    let TtsConfig::Mcp(tts) = config.tts else {
        panic!("expected mcp tts");
    };
    assert_eq!(tts.url, DEFAULT_MCP_URL);
    assert_eq!(config.transcripts.drop, vec!["[BLANK_AUDIO]".to_string()]);
    assert_eq!(config.transcripts.short_utterance_ms, 600);
}

#[test]
fn unknown_backend_is_an_error() {
    let err = LocalVoiceConfig::from_toml_str("[stt]\nbackend = \"cloud\"\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("cloud"), "{err}");
}
