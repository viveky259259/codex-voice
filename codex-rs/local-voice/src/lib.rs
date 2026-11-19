//! Local voice engine: microphone capture through a command, energy-based
//! voice activity detection, local speech-to-text, and local text-to-speech
//! with half-duplex echo suppression. No network services beyond the local
//! ones you configure; no native audio libraries.
//!
//! ```ignore
//! let (voice, mut events) = LocalVoice::start(LocalVoiceConfig::default());
//! while let Some(event) = events.recv().await {
//!     if let LocalVoiceEvent::Transcript { text } = event {
//!         voice.speak(format!("You said {text}"));
//!     }
//! }
//! ```

mod capture;
mod config;
mod engine;
mod error;
pub mod mcp;
pub mod model_vad;
mod process;
mod stt;
mod template;
mod transcript_filter;
mod tts;
pub mod vad;
mod wav;

#[cfg(test)]
mod test_support;

pub use capture::Capture;
pub use capture::CaptureExit;
pub use config::CaptureConfig;
pub use config::CommandSttConfig;
pub use config::CommandToFileTtsConfig;
pub use config::CommandTtsConfig;
pub use config::DEFAULT_MCP_URL;
pub use config::LocalVoiceConfig;
pub use config::McpJobConfig;
pub use config::McpSttConfig;
pub use config::McpTtsConfig;
pub use config::OpenAiSttConfig;
pub use config::OpenAiTtsConfig;
pub use config::PlaybackConfig;
pub use config::SttConfig;
pub use config::TranscriptFilterConfig;
pub use config::TtsConfig;
pub use engine::LocalVoice;
pub use engine::LocalVoiceEvent;
pub use engine::LocalVoiceHandle;
pub use error::Result;
pub use error::VoiceError;
pub use error::VoiceStage;
pub use stt::CommandStt;
pub use stt::McpStt;
pub use stt::OpenAiStt;
pub use stt::SpeechToText;
pub use stt::build_stt;
pub use transcript_filter::clean_transcript;
pub use tts::CommandToFileTts;
pub use tts::CommandTts;
pub use tts::McpTts;
pub use tts::OpenAiTts;
pub use tts::Speech;
pub use tts::TextToSpeech;
pub use tts::build_tts;
pub use tts::prepare_text;
pub use vad::VadConfig;
pub use wav::encode_wav;
pub use wav::write_wav;
