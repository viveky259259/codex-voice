//! Speech-to-text backends.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::header::AUTHORIZATION;
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;

use crate::config::CommandSttConfig;
use crate::config::McpSttConfig;
use crate::config::OpenAiSttConfig;
use crate::config::SttConfig;
use crate::error::Result;
use crate::error::VoiceError;
use crate::mcp::McpClient;
use crate::mcp::excerpt;
use crate::mcp::find_string;
use crate::mcp::local_http_client;
use crate::process::run_command;
use crate::template::argv_uses;
use crate::template::substitute_argv;
use crate::template::substitute_json;

/// Turns a 16 kHz mono wav file into text.
pub trait SpeechToText: Send + Sync {
    /// Checks the backend once at startup (e.g. MCP tool discovery).
    fn prepare(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// `scratch` is an empty private directory the backend may use.
    fn transcribe<'a>(&'a self, wav: &'a Path, scratch: &'a Path) -> BoxFuture<'a, Result<String>>;
}

pub fn build_stt(config: &SttConfig) -> Result<Arc<dyn SpeechToText>> {
    Ok(match config {
        SttConfig::Command(config) => Arc::new(CommandStt::new(config.clone())?),
        SttConfig::OpenaiCompatible(config) => Arc::new(OpenAiStt::new(config.clone())?),
        SttConfig::Mcp(config) => Arc::new(McpStt::new(config.clone())?),
    })
}

pub struct CommandStt {
    config: CommandSttConfig,
}

impl CommandStt {
    pub fn new(config: CommandSttConfig) -> Result<Self> {
        if config.command.is_empty() {
            return Err(VoiceError::Config("stt.command is empty".to_string()));
        }
        Ok(Self { config })
    }

    async fn run(&self, wav: &Path, scratch: &Path) -> Result<String> {
        let out_file = scratch.join("transcript.txt");
        let out_dir = scratch.join("out");
        let uses_out = argv_uses(&self.config.command, "out");
        let uses_outdir = argv_uses(&self.config.command, "outdir");
        if uses_outdir {
            tokio::fs::create_dir_all(&out_dir).await?;
        }
        let wav = wav.to_string_lossy();
        let out = out_file.to_string_lossy();
        let outdir = out_dir.to_string_lossy();
        let argv = substitute_argv(
            &self.config.command,
            &[
                ("wav", wav.as_ref()),
                ("out", out.as_ref()),
                ("outdir", outdir.as_ref()),
            ],
        );
        let output = run_command(&argv, self.config.timeout_ms, !uses_out && !uses_outdir).await?;
        if uses_out {
            return Ok(tokio::fs::read_to_string(&out_file)
                .await
                .map_err(|err| VoiceError::Command(format!("transcript file {out}: {err}")))?);
        }
        if uses_outdir {
            return read_text_file_in(&out_dir).await;
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The `.txt` file a command wrote into `dir` (or the only file there).
async fn read_text_file_in(dir: &Path) -> Result<String> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        files.push(entry.path());
    }
    files.sort();
    let chosen = files
        .iter()
        .find(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .or_else(|| (files.len() == 1).then(|| &files[0]))
        .ok_or_else(|| {
            VoiceError::Command(format!(
                "no transcript .txt file written to {}",
                dir.display()
            ))
        })?;
    Ok(tokio::fs::read_to_string(chosen).await?)
}

impl SpeechToText for CommandStt {
    fn transcribe<'a>(&'a self, wav: &'a Path, scratch: &'a Path) -> BoxFuture<'a, Result<String>> {
        Box::pin(self.run(wav, scratch))
    }
}

pub struct OpenAiStt {
    config: OpenAiSttConfig,
    http: reqwest::Client,
}

impl OpenAiStt {
    pub fn new(config: OpenAiSttConfig) -> Result<Self> {
        Ok(Self {
            config,
            http: local_http_client()?,
        })
    }

    async fn run(&self, wav: &Path) -> Result<String> {
        let audio = tokio::fs::read(wav).await?;
        let boundary = format!("codex-local-voice-{:016x}", random_u64());
        let mut fields = vec![
            ("model", self.config.model.clone()),
            ("response_format", "json".to_string()),
        ];
        if let Some(language) = &self.config.language {
            fields.push(("language", language.clone()));
        }
        let body = multipart_body(&boundary, &fields, "utterance.wav", &audio);
        let url = endpoint(&self.config.base_url, &self.config.path);
        let mut request = self
            .http
            .post(&url)
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .timeout(Duration::from_millis(self.config.timeout_ms))
            .body(body);
        if let Some(key) = api_key(self.config.api_key_env.as_deref()) {
            request = request.header(AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = request
            .send()
            .await
            .map_err(|err| VoiceError::Http(format!("POST {url}: {err}")))?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(VoiceError::Http(format!(
                "POST {url}: {status}: {}",
                excerpt(&text)
            )));
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(value @ Value::Object(_)) => find_string(&value, None, &["text", "transcript"])
                .or_else(|| value.get("text").map(|_| String::new()))
                .ok_or_else(|| {
                    VoiceError::Http(format!("no `text` in response: {}", excerpt(&text)))
                }),
            _ => Ok(text),
        }
    }
}

impl SpeechToText for OpenAiStt {
    fn transcribe<'a>(
        &'a self,
        wav: &'a Path,
        _scratch: &'a Path,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(self.run(wav))
    }
}

pub struct McpStt {
    config: McpSttConfig,
    client: McpClient,
}

const TRANSCRIPT_PATHS: &[&str] = &[
    "text",
    "transcript",
    "result.text",
    "result.transcript",
    "output.text",
    "output.transcript",
    "job.result.text",
    "result",
    "output",
];

impl McpStt {
    pub fn new(config: McpSttConfig) -> Result<Self> {
        let client = McpClient::new(config.url.clone())?;
        Ok(Self { config, client })
    }

    async fn check(&self) -> Result<()> {
        self.client
            .require_tools(&[
                ("stt.tool", self.config.tool.as_str()),
                ("stt.job.tool", self.config.job.tool.as_str()),
            ])
            .await
    }

    async fn run(&self, wav: &Path) -> Result<String> {
        self.check().await?;
        let wav = wav.to_string_lossy();
        let arguments = substitute_json(&self.config.arguments, &[("wav", wav.as_ref())]);
        let text_path = self.config.text_path.as_deref();
        let done = self
            .client
            .run_job(&self.config.tool, arguments, &self.config.job, |value| {
                find_string(value, text_path, TRANSCRIPT_PATHS).is_some()
            })
            .await?;
        find_string(&done, text_path, TRANSCRIPT_PATHS).ok_or_else(|| {
            VoiceError::Mcp(format!(
                "no transcript in the job result (set stt.text_path): {}",
                excerpt(&done.to_string())
            ))
        })
    }
}

impl SpeechToText for McpStt {
    fn prepare(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.check())
    }

    fn transcribe<'a>(
        &'a self,
        wav: &'a Path,
        _scratch: &'a Path,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(self.run(wav))
    }
}

pub(crate) fn endpoint(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    match (base.strip_suffix("/v1"), path.strip_prefix("/v1/")) {
        (Some(_), Some(_)) => format!("{base}{}", &path[3..]),
        _ => format!("{base}{path}"),
    }
}

pub(crate) fn api_key(env_var: Option<&str>) -> Option<String> {
    let value = std::env::var(env_var?).ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn multipart_body(
    boundary: &str,
    fields: &[(&str, String)],
    filename: &str,
    file: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(file.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn random_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    RandomState::new().hash_one(std::time::SystemTime::now())
}

#[cfg(test)]
#[path = "stt_tests.rs"]
mod tests;
