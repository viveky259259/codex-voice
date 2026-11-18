//! Serde configuration for the local voice engine.
//!
//! Every table has defaults chosen for the current OS, so an empty TOML
//! document is a valid configuration.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::error::Result;
use crate::error::VoiceError;
use crate::vad::VadConfig;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalVoiceConfig {
    pub capture: CaptureConfig,
    pub vad: VadConfig,
    pub stt: SttConfig,
    pub tts: TtsConfig,
    pub playback: PlaybackConfig,
    pub transcripts: TranscriptFilterConfig,
}

impl LocalVoiceConfig {
    /// Parses a TOML document (all tables optional).
    pub fn from_toml_str(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|err| VoiceError::Config(err.to_string()))
    }
}

/// Microphone capture: a command that writes raw s16le mono PCM at 16 kHz
/// to stdout. `{device}` in the command is replaced by `device`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    /// When false, no microphone is opened (speak-only mode).
    pub enabled: bool,
    pub command: Vec<String>,
    pub device: String,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        let (input_format, device, input) = if cfg!(target_os = "macos") {
            ("avfoundation", "0", ":{device}")
        } else if cfg!(target_os = "windows") {
            ("dshow", "default", "audio={device}")
        } else {
            ("pulse", "default", "{device}")
        };
        let command = [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            input_format,
            "-i",
            input,
            "-ac",
            "1",
            "-ar",
            "16000",
            "-f",
            "s16le",
            "-",
        ];
        Self {
            enabled: true,
            command: strings(&command),
            device: device.to_string(),
        }
    }
}

/// Speech-to-text backend, selected by `backend = "..."`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum SttConfig {
    Command(CommandSttConfig),
    OpenaiCompatible(OpenAiSttConfig),
    Mcp(McpSttConfig),
}

impl Default for SttConfig {
    fn default() -> Self {
        SttConfig::Command(CommandSttConfig::default())
    }
}

/// Runs a command per utterance. Placeholders: `{wav}` (the utterance),
/// `{outdir}` (an empty directory; the transcript is the `.txt` file the
/// command writes there), `{out}` (a text file path the command writes).
/// Without `{out}`/`{outdir}` the transcript is read from stdout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandSttConfig {
    pub command: Vec<String>,
    pub timeout_ms: u64,
}

impl Default for CommandSttConfig {
    fn default() -> Self {
        let command: &[&str] = if cfg!(target_os = "macos") {
            &[
                "mlx_whisper",
                "{wav}",
                "--model",
                "mlx-community/whisper-large-v3-turbo",
                "--language",
                "en",
                "--output-format",
                "txt",
                "--output-dir",
                "{outdir}",
                "--verbose",
                "False",
            ]
        } else {
            &[
                "whisper-cli",
                "-m",
                "models/ggml-base.en.bin",
                "-f",
                "{wav}",
                "-nt",
            ]
        };
        Self {
            command: strings(command),
            timeout_ms: 60_000,
        }
    }
}

/// `POST {base_url}{path}` as multipart (OpenAI audio transcriptions API).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiSttConfig {
    pub base_url: String,
    pub path: String,
    pub model: String,
    pub language: Option<String>,
    /// Name of an environment variable holding a bearer token, if any.
    pub api_key_env: Option<String>,
    pub timeout_ms: u64,
}

impl Default for OpenAiSttConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8178".to_string(),
            path: "/v1/audio/transcriptions".to_string(),
            model: "whisper-1".to_string(),
            language: None,
            api_key_env: None,
            timeout_ms: 60_000,
        }
    }
}

/// Transcription through an MCP server over streamable HTTP.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpSttConfig {
    pub url: String,
    /// Tool that starts a transcription.
    pub tool: String,
    /// Arguments template; string values may contain `{wav}`.
    pub arguments: Value,
    /// Dotted path of the transcript in the final job result (e.g.
    /// `result.text`). Common fields are tried when unset or missing.
    pub text_path: Option<String>,
    pub job: McpJobConfig,
}

impl Default for McpSttConfig {
    fn default() -> Self {
        Self {
            url: DEFAULT_MCP_URL.to_string(),
            tool: "transcribe".to_string(),
            arguments: json!({ "model_id": "whisper:base", "input_path": "{wav}" }),
            text_path: None,
            job: McpJobConfig::default(),
        }
    }
}

pub const DEFAULT_MCP_URL: &str = "http://127.0.0.1:5051/mcp";

/// How a tool call that returns a job id is polled to completion. When the
/// call result has no job id it is taken as the final result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpJobConfig {
    pub tool: String,
    /// Arguments template; string values may contain `{job_id}`.
    pub arguments: Value,
    pub job_id_path: Option<String>,
    pub status_path: Option<String>,
    pub done_statuses: Vec<String>,
    pub failed_statuses: Vec<String>,
    pub poll_interval_ms: u64,
    pub timeout_ms: u64,
}

impl Default for McpJobConfig {
    fn default() -> Self {
        Self {
            tool: "get_job".to_string(),
            arguments: json!({ "job_id": "{job_id}" }),
            job_id_path: None,
            status_path: None,
            done_statuses: strings(&[
                "done",
                "completed",
                "complete",
                "succeeded",
                "success",
                "finished",
                "ok",
            ]),
            failed_statuses: strings(&["failed", "error", "errored", "cancelled", "canceled"]),
            poll_interval_ms: 250,
            timeout_ms: 120_000,
        }
    }
}

/// Text-to-speech backend, selected by `backend = "..."`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum TtsConfig {
    Command(CommandTtsConfig),
    CommandToFile(CommandToFileTtsConfig),
    OpenaiCompatible(OpenAiTtsConfig),
    Mcp(McpTtsConfig),
}

impl Default for TtsConfig {
    fn default() -> Self {
        TtsConfig::Command(CommandTtsConfig::default())
    }
}

/// A command that speaks `{text}` itself (one argv element, no shell).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandTtsConfig {
    pub command: Vec<String>,
}

impl Default for CommandTtsConfig {
    fn default() -> Self {
        let command: &[&str] = if cfg!(target_os = "macos") {
            &["say", "{text}"]
        } else {
            &["espeak-ng", "{text}"]
        };
        Self {
            command: strings(command),
        }
    }
}

/// A command that writes `{text}` as audio to `{wav}`; the player plays it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandToFileTtsConfig {
    pub command: Vec<String>,
    pub timeout_ms: u64,
}

impl Default for CommandToFileTtsConfig {
    fn default() -> Self {
        let command: &[&str] = if cfg!(target_os = "macos") {
            &["say", "-o", "{wav}", "--data-format=LEI16@22050", "{text}"]
        } else {
            &["espeak-ng", "-w", "{wav}", "{text}"]
        };
        Self {
            command: strings(command),
            timeout_ms: 60_000,
        }
    }
}

/// `POST {base_url}{path}` with an OpenAI speech request (kokoro-fastapi).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiTtsConfig {
    pub base_url: String,
    pub path: String,
    pub model: String,
    pub voice: String,
    pub speed: Option<f32>,
    pub api_key_env: Option<String>,
    pub timeout_ms: u64,
}

impl Default for OpenAiTtsConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8880".to_string(),
            path: "/v1/audio/speech".to_string(),
            model: "kokoro".to_string(),
            voice: "af_heart".to_string(),
            speed: None,
            api_key_env: None,
            timeout_ms: 60_000,
        }
    }
}

/// Speech through an MCP server: the job's artifact (a wav path or URL) is
/// played with the player.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpTtsConfig {
    pub url: String,
    pub tool: String,
    /// Arguments template; string values may contain `{text}` and `{wav}`
    /// (a suggested output path in the private temp dir).
    pub arguments: Value,
    /// Dotted path of the audio file in the final job result. Common fields
    /// and any string ending in `.wav` are tried when unset or missing.
    pub audio_path: Option<String>,
    /// Directory that relative artifact paths are resolved against.
    pub artifact_dir: Option<String>,
    pub job: McpJobConfig,
}

impl Default for McpTtsConfig {
    fn default() -> Self {
        Self {
            url: DEFAULT_MCP_URL.to_string(),
            tool: "speak".to_string(),
            arguments: json!({
                "model_id": "hf:mlx-community/Kokoro-82M-8bit",
                "voice": "af_heart",
                "text": "{text}",
            }),
            audio_path: None,
            artifact_dir: None,
            job: McpJobConfig::default(),
        }
    }
}

/// Playback and half-duplex behaviour.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackConfig {
    /// Plays `{wav}` for the file-based TTS backends.
    pub player: Vec<String>,
    /// Text longer than this is cut (at a word boundary) before speaking.
    pub max_chars: usize,
    /// Microphone audio is dropped while speaking and for this long after.
    pub echo_tail_ms: u64,
    /// Sustained loud speech during playback stops it and starts an utterance.
    pub barge_in: bool,
    /// Barge-in level over the noise floor, in dB.
    pub barge_in_threshold_db: f32,
    /// How long the barge-in level must be held.
    pub barge_in_min_ms: u32,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        let player: &[&str] = if cfg!(target_os = "macos") {
            &["afplay", "{wav}"]
        } else {
            &["aplay", "-q", "{wav}"]
        };
        Self {
            player: strings(player),
            max_chars: 400,
            echo_tail_ms: 400,
            barge_in: false,
            barge_in_threshold_db: 24.0,
            barge_in_min_ms: 300,
        }
    }
}

/// Transcripts that are dropped instead of becoming a user turn. Matching
/// ignores case, surrounding whitespace and trailing punctuation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscriptFilterConfig {
    /// Always dropped.
    pub drop: Vec<String>,
    /// Dropped when the voiced part of the utterance is shorter than
    /// `short_utterance_ms` (typical Whisper hallucinations on noise).
    pub drop_if_short: Vec<String>,
    pub short_utterance_ms: u32,
    /// Drop transcripts that are entirely `[...]` or `(...)` annotations.
    pub drop_bracketed: bool,
    /// Drop transcripts with at least `repeat_min_words` words of which at
    /// most this fraction are distinct: Whisper loops one phrase on noise
    /// ("Það er það er það er ..."). 0 disables.
    pub max_distinct_word_ratio: f32,
    pub repeat_min_words: usize,
}

impl Default for TranscriptFilterConfig {
    fn default() -> Self {
        Self {
            drop: strings(&[
                "[BLANK_AUDIO]",
                "(silence)",
                "[silence]",
                "(blank audio)",
                "[no speech]",
                "[inaudible]",
                "[music]",
                "(music)",
            ]),
            // Whisper's usual phantoms on noise. Real one-word answers
            // ("yes", "no", "approve", "confirm") are not in this list.
            drop_if_short: strings(&[
                "Thank you.",
                "Thank you very much.",
                "Thank you so much.",
                "Thanks.",
                "Thanks for watching!",
                "Thank you for watching.",
                "Please subscribe.",
                "You",
                "Bye.",
                "Hmm.",
                "Um.",
                "Uh.",
            ]),
            short_utterance_ms: 1500,
            drop_bracketed: true,
            max_distinct_word_ratio: 0.3,
            repeat_min_words: 8,
        }
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
