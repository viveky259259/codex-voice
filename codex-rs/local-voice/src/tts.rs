//! Text-to-speech backends.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::header::AUTHORIZATION;
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;
use serde_json::json;

use crate::config::CommandToFileTtsConfig;
use crate::config::CommandTtsConfig;
use crate::config::McpTtsConfig;
use crate::config::OpenAiTtsConfig;
use crate::config::TtsConfig;
use crate::error::Result;
use crate::error::VoiceError;
use crate::mcp::McpClient;
use crate::mcp::excerpt;
use crate::mcp::find_string;
use crate::mcp::local_http_client;
use crate::process::run_command;
use crate::stt::api_key;
use crate::stt::endpoint;
use crate::template::substitute_argv;
use crate::template::substitute_json;

/// What to do to make the text heard.
#[derive(Clone, Debug, PartialEq)]
pub enum Speech {
    /// Run this command; it speaks by itself (e.g. `say`).
    Command(Vec<String>),
    /// Play this wav file with the configured player.
    Wav { path: PathBuf, delete_after: bool },
}

pub trait TextToSpeech: Send + Sync {
    fn prepare(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// `out_wav` is a free path in the private temp dir for file output.
    fn synthesize<'a>(&'a self, text: &'a str, out_wav: &'a Path) -> BoxFuture<'a, Result<Speech>>;
}

pub fn build_tts(config: &TtsConfig) -> Result<Arc<dyn TextToSpeech>> {
    Ok(match config {
        TtsConfig::Command(config) => Arc::new(CommandTts::new(config.clone())?),
        TtsConfig::CommandToFile(config) => Arc::new(CommandToFileTts::new(config.clone())?),
        TtsConfig::OpenaiCompatible(config) => Arc::new(OpenAiTts::new(config.clone())?),
        TtsConfig::Mcp(config) => Arc::new(McpTts::new(config.clone())?),
    })
}

/// Trims, drops leading dashes (so the text is never read as an option)
/// and caps the length at a word or sentence boundary.
pub fn prepare_text(text: &str, max_chars: usize) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_start_matches(|c: char| c == '-' || c.is_whitespace());
    if text.is_empty() {
        return None;
    }
    let max_chars = max_chars.max(1);
    let Some((cut, _)) = text.char_indices().nth(max_chars) else {
        return Some(text.to_string());
    };
    let head = &text[..cut];
    let sentence_end = head
        .rfind(['.', '!', '?'])
        .filter(|index| *index >= head.len() / 2)
        .map(|index| index + 1);
    let word_end = head.rfind(' ').filter(|index| *index > 0);
    let end = sentence_end.or(word_end).unwrap_or(cut);
    Some(head[..end].trim_end().to_string())
}

pub struct CommandTts {
    config: CommandTtsConfig,
}

impl CommandTts {
    pub fn new(config: CommandTtsConfig) -> Result<Self> {
        if config.command.is_empty() {
            return Err(VoiceError::Config("tts.command is empty".to_string()));
        }
        Ok(Self { config })
    }
}

impl TextToSpeech for CommandTts {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        _out_wav: &'a Path,
    ) -> BoxFuture<'a, Result<Speech>> {
        let argv = substitute_argv(&self.config.command, &[("text", text)]);
        Box::pin(async move { Ok(Speech::Command(argv)) })
    }
}

pub struct CommandToFileTts {
    config: CommandToFileTtsConfig,
}

impl CommandToFileTts {
    pub fn new(config: CommandToFileTtsConfig) -> Result<Self> {
        if config.command.is_empty() {
            return Err(VoiceError::Config("tts.command is empty".to_string()));
        }
        Ok(Self { config })
    }

    async fn run(&self, text: &str, out_wav: &Path) -> Result<Speech> {
        let wav = out_wav.to_string_lossy();
        let argv = substitute_argv(
            &self.config.command,
            &[("text", text), ("wav", wav.as_ref())],
        );
        run_command(&argv, self.config.timeout_ms, false).await?;
        if !tokio::fs::try_exists(out_wav).await.unwrap_or(false) {
            return Err(VoiceError::Command(format!(
                "`{}` did not write {wav}",
                argv.first().map(String::as_str).unwrap_or_default()
            )));
        }
        Ok(Speech::Wav {
            path: out_wav.to_path_buf(),
            delete_after: true,
        })
    }
}

impl TextToSpeech for CommandToFileTts {
    fn synthesize<'a>(&'a self, text: &'a str, out_wav: &'a Path) -> BoxFuture<'a, Result<Speech>> {
        Box::pin(self.run(text, out_wav))
    }
}

pub struct OpenAiTts {
    config: OpenAiTtsConfig,
    http: reqwest::Client,
}

impl OpenAiTts {
    pub fn new(config: OpenAiTtsConfig) -> Result<Self> {
        Ok(Self {
            config,
            http: local_http_client()?,
        })
    }

    async fn run(&self, text: &str, out_wav: &Path) -> Result<Speech> {
        let url = endpoint(&self.config.base_url, &self.config.path);
        let mut body = json!({
            "model": self.config.model,
            "input": text,
            "voice": self.config.voice,
            "response_format": "wav",
        });
        if let Some(speed) = self.config.speed {
            body["speed"] = json!(speed);
        }
        let mut request = self
            .http
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .timeout(Duration::from_millis(self.config.timeout_ms))
            .body(body.to_string());
        if let Some(key) = api_key(self.config.api_key_env.as_deref()) {
            request = request.header(AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = request
            .send()
            .await
            .map_err(|err| VoiceError::Http(format!("POST {url}: {err}")))?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(VoiceError::Http(format!(
                "POST {url}: {status}: {}",
                excerpt(&String::from_utf8_lossy(&bytes))
            )));
        }
        tokio::fs::write(out_wav, &bytes).await?;
        Ok(Speech::Wav {
            path: out_wav.to_path_buf(),
            delete_after: true,
        })
    }
}

impl TextToSpeech for OpenAiTts {
    fn synthesize<'a>(&'a self, text: &'a str, out_wav: &'a Path) -> BoxFuture<'a, Result<Speech>> {
        Box::pin(self.run(text, out_wav))
    }
}

pub struct McpTts {
    config: McpTtsConfig,
    client: McpClient,
    http: reqwest::Client,
}

const AUDIO_PATHS: &[&str] = &[
    "audio_path",
    "output_path",
    "artifact_path",
    "wav_path",
    "path",
    "result.audio_path",
    "result.output_path",
    "result.artifact_path",
    "result.path",
    "artifact.path",
    "artifacts.0.path",
    "artifacts.0.uri",
    "artifacts.0.url",
    "artifacts.0",
    "result.artifacts.0.path",
    "result.artifacts.0.uri",
    "result.artifacts.0",
];

impl McpTts {
    pub fn new(config: McpTtsConfig) -> Result<Self> {
        let client = McpClient::new(config.url.clone())?;
        Ok(Self {
            config,
            client,
            http: local_http_client()?,
        })
    }

    async fn check(&self) -> Result<()> {
        self.client
            .require_tools(&[
                ("tts.tool", self.config.tool.as_str()),
                ("tts.job.tool", self.config.job.tool.as_str()),
            ])
            .await
    }

    fn find_audio(&self, value: &Value) -> Option<String> {
        find_string(value, self.config.audio_path.as_deref(), AUDIO_PATHS)
            .filter(|found| looks_like_audio_location(found))
            .or_else(|| find_wav_string(value))
    }

    async fn run(&self, text: &str, out_wav: &Path) -> Result<Speech> {
        self.check().await?;
        let wav = out_wav.to_string_lossy();
        let arguments = substitute_json(
            &self.config.arguments,
            &[("text", text), ("wav", wav.as_ref())],
        );
        let done = self
            .client
            .run_job(&self.config.tool, arguments, &self.config.job, |value| {
                self.find_audio(value).is_some()
            })
            .await?;
        let location = self.find_audio(&done).ok_or_else(|| {
            VoiceError::Mcp(format!(
                "no audio file in the job result (set tts.audio_path): {}",
                excerpt(&done.to_string())
            ))
        })?;
        if location.starts_with("http://") || location.starts_with("https://") {
            let response = self.http.get(&location).send().await?;
            let status = response.status();
            let bytes = response.bytes().await?;
            if !status.is_success() {
                return Err(VoiceError::Http(format!("GET {location}: {status}")));
            }
            tokio::fs::write(out_wav, &bytes).await?;
            return Ok(Speech::Wav {
                path: out_wav.to_path_buf(),
                delete_after: true,
            });
        }
        let raw = location.strip_prefix("file://").unwrap_or(&location);
        let mut path = PathBuf::from(raw);
        if path.is_relative()
            && let Some(dir) = &self.config.artifact_dir
        {
            path = Path::new(dir).join(path);
        }
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Err(VoiceError::Mcp(format!(
                "audio artifact {} does not exist (set tts.artifact_dir for relative paths)",
                path.display()
            )));
        }
        // The server owns its artifact unless it wrote to our suggested path.
        let delete_after = path == out_wav;
        Ok(Speech::Wav { path, delete_after })
    }
}

impl TextToSpeech for McpTts {
    fn prepare(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.check())
    }

    fn synthesize<'a>(&'a self, text: &'a str, out_wav: &'a Path) -> BoxFuture<'a, Result<Speech>> {
        Box::pin(self.run(text, out_wav))
    }
}

fn looks_like_audio_location(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with('/')
        || lower.starts_with("file://")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || [".wav", ".mp3", ".aiff", ".flac", ".ogg", ".m4a"]
            .iter()
            .any(|ext| lower.ends_with(ext))
}

/// Any string in the value that ends in `.wav`, depth first.
fn find_wav_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if text.to_ascii_lowercase().ends_with(".wav") => Some(text.clone()),
        Value::Array(items) => items.iter().find_map(find_wav_string),
        Value::Object(map) => map.values().find_map(find_wav_string),
        _ => None,
    }
}

#[cfg(test)]
#[path = "tts_tests.rs"]
mod tests;
