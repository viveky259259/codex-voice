use std::fmt;

use serde::Deserialize;
use serde::Serialize;

/// The part of the voice pipeline an error came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceStage {
    Config,
    Capture,
    Stt,
    Tts,
    Playback,
}

impl fmt::Display for VoiceStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            VoiceStage::Config => "config",
            VoiceStage::Capture => "capture",
            VoiceStage::Stt => "speech-to-text",
            VoiceStage::Tts => "text-to-speech",
            VoiceStage::Playback => "playback",
        };
        f.write_str(name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("{0}")]
    Command(String),
    #[error("{0}")]
    Http(String),
    #[error("{0}")]
    Mcp(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("timed out after {0} ms")]
    Timeout(u64),
    #[error("interrupted")]
    Interrupted,
    #[error("the voice engine has shut down")]
    Closed,
}

pub type Result<T> = std::result::Result<T, VoiceError>;

impl From<reqwest::Error> for VoiceError {
    fn from(err: reqwest::Error) -> Self {
        VoiceError::Http(err.to_string())
    }
}
