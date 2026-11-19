//! Model-based voice activity detection.
//!
//! A long-running local helper (for example `helpers/silero_vad.py`, Silero
//! VAD v5 on onnxruntime) reads 16 kHz mono s16le audio on stdin in frames of
//! [`ModelVadConfig::frame_samples`] and writes one line per frame on stdout:
//! the probability (0.0..1.0) that the frame holds speech. It writes `ready`
//! first, once its model is loaded.
//!
//! [`ProbabilityVad`] turns those probabilities into the same [`VadEvent`]s as
//! the energy detector, with hysteresis: speech starts once
//! `min_speech_ms` of frames reach `start_probability`, continues while frames
//! stay at `continue_probability`, and ends after `end_silence_ms` below it.
//! A trained model tells voice from hiss, fans and hum by its spectrum, so
//! there is no noise floor to adapt and steady speech is never mistaken for
//! noise.

use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::Lines;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;

use crate::error::Result;
use crate::error::VoiceError;
use crate::process::ProcessGuard;
use crate::process::build_command;
use crate::process::program_name;
use crate::vad::SAMPLE_RATE;
use crate::vad::Utterance;
use crate::vad::VadConfig;
use crate::vad::VadEvent;

/// The bundled helper, written to the cache directory on first use so an
/// installed binary needs no repository checkout.
const BUNDLED_HELPER: &str = include_str!("../helpers/silero_vad.py");
const BUNDLED_HELPER_NAME: &str = "silero_vad.py";
/// `auto` gives up on the model after this long and uses the energy
/// detector, so a first run that installs dependencies can't stall voice.
const AUTO_STARTUP_TIMEOUT_MS: u64 = 60_000;

/// Which detector decides where utterances start and end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VadBackend {
    /// The model on macOS when `uv` is installed, otherwise energy. If the
    /// model can't start (offline first run, timeout), falls back to energy
    /// with a warning instead of failing.
    #[default]
    Auto,
    /// Energy over an adaptive noise floor (no extra dependencies).
    Energy,
    /// Speech probabilities from a local model helper (`[vad.model]`).
    /// Fails, rather than falling back, if the helper can't start.
    Model,
}

/// What [`plan`] decided to run.
#[derive(Clone, Debug, PartialEq)]
pub enum DetectorPlan {
    Energy,
    Model {
        command: Vec<String>,
        /// On a start failure, use the energy detector instead of failing.
        fallback_to_energy: bool,
        startup_timeout_ms: u64,
    },
}

/// The host facts [`plan`] depends on, injected so every branch is testable.
#[derive(Clone, Debug)]
pub struct Host {
    pub macos: bool,
    /// `uv`, if found.
    pub uv: Option<std::path::PathBuf>,
    /// Where the bundled helper is written.
    pub cache_dir: Option<std::path::PathBuf>,
}

impl Host {
    pub fn current() -> Self {
        Self {
            macos: cfg!(target_os = "macos"),
            uv: find_uv(),
            cache_dir: cache_dir(),
        }
    }
}

/// Picks the detector for `config` on `host`.
pub fn plan(config: &VadConfig, host: &Host) -> Result<DetectorPlan> {
    let model = &config.model;
    match config.backend {
        VadBackend::Energy => Ok(DetectorPlan::Energy),
        VadBackend::Model => Ok(DetectorPlan::Model {
            command: match model.command.is_empty() {
                false => model.command.clone(),
                true => bundled_command(host)?,
            },
            fallback_to_energy: false,
            startup_timeout_ms: model.startup_timeout_ms,
        }),
        VadBackend::Auto => {
            if !model.command.is_empty() {
                return Ok(DetectorPlan::Model {
                    command: model.command.clone(),
                    fallback_to_energy: true,
                    startup_timeout_ms: model.startup_timeout_ms.min(AUTO_STARTUP_TIMEOUT_MS),
                });
            }
            if !host.macos {
                return Ok(DetectorPlan::Energy);
            }
            match bundled_command(host) {
                Ok(command) => Ok(DetectorPlan::Model {
                    command,
                    fallback_to_energy: true,
                    startup_timeout_ms: model.startup_timeout_ms.min(AUTO_STARTUP_TIMEOUT_MS),
                }),
                Err(err) => {
                    tracing::info!("vad: using the energy detector ({err})");
                    Ok(DetectorPlan::Energy)
                }
            }
        }
    }
}

/// `uv run --quiet --script <cache>/silero_vad.py`, writing the bundled
/// helper there if it is missing or out of date.
fn bundled_command(host: &Host) -> Result<Vec<String>> {
    let Some(uv) = &host.uv else {
        return Err(VoiceError::Config(
            "the speech detection model needs `uv` (https://docs.astral.sh/uv/), or set [vad.model] command"
                .to_string(),
        ));
    };
    let Some(dir) = &host.cache_dir else {
        return Err(VoiceError::Config(
            "no cache directory for the speech detection helper (set $CODEX_VOICE_MODELS)"
                .to_string(),
        ));
    };
    let helper = write_bundled_helper(dir)?;
    Ok(vec![
        uv.to_string_lossy().into_owned(),
        "run".to_string(),
        "--quiet".to_string(),
        "--script".to_string(),
        helper.to_string_lossy().into_owned(),
    ])
}

/// Writes the bundled helper into `dir` unless an identical copy is there.
pub fn write_bundled_helper(dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let path = dir.join(BUNDLED_HELPER_NAME);
    if std::fs::read_to_string(&path).is_ok_and(|current| current == BUNDLED_HELPER) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir)?;
    let partial = dir.join(format!("{BUNDLED_HELPER_NAME}.{}.part", std::process::id()));
    std::fs::write(&partial, BUNDLED_HELPER)?;
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

/// `$CODEX_VOICE_MODELS`, else `~/.cache/codex-voice` (where the helper also
/// keeps the model).
fn cache_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_VOICE_MODELS").filter(|dir| !dir.is_empty()) {
        return Some(dir.into());
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| {
            std::path::PathBuf::from(home)
                .join(".cache")
                .join("codex-voice")
        })
}

/// `uv` on `PATH`, else its usual install locations (a GUI-launched terminal
/// may have a short `PATH`).
fn find_uv() -> Option<std::path::PathBuf> {
    let on_path = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("uv"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let usual = [
        Some(std::path::PathBuf::from("/opt/homebrew/bin/uv")),
        Some(std::path::PathBuf::from("/usr/local/bin/uv")),
        home.as_ref().map(|home| home.join(".local/bin/uv")),
        home.as_ref().map(|home| home.join(".cargo/bin/uv")),
    ];
    on_path
        .into_iter()
        .chain(usual.into_iter().flatten())
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelVadConfig {
    /// The helper, as argv (never through a shell). Empty (the default) runs
    /// the bundled Silero helper with `uv`:
    /// `["uv", "run", "--quiet", "--script", "<cache>/silero_vad.py"]`.
    pub command: Vec<String>,
    /// Samples per frame the helper expects (Silero v5 at 16 kHz: 512).
    pub frame_samples: usize,
    /// A frame at or above this probability may start speech.
    pub start_probability: f32,
    /// Speech continues while frames stay at or above this probability.
    pub continue_probability: f32,
    /// How long to wait for the helper's `ready` line. The first run of a
    /// `uv` script installs its dependencies and downloads the model.
    pub startup_timeout_ms: u64,
}

impl Default for ModelVadConfig {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            frame_samples: 512,
            start_probability: 0.5,
            continue_probability: 0.35,
            startup_timeout_ms: 120_000,
        }
    }
}

#[derive(Debug)]
enum State {
    Idle,
    /// Voiced frames seen, not yet `min_speech_ms` of them.
    Candidate {
        voiced: u32,
    },
    Speech {
        /// Frames since the first voiced frame.
        frames: u32,
        /// Frame count at the last voiced frame.
        last_voiced: u32,
        unvoiced: u32,
    },
}

/// Hysteresis over per-frame speech probabilities. Pure and synchronous.
#[derive(Debug)]
pub struct ProbabilityVad {
    frame_len: usize,
    start_probability: f32,
    continue_probability: f32,
    min_speech_frames: u32,
    end_silence_frames: u32,
    max_samples: usize,
    pre_roll: VecDeque<i16>,
    pre_roll_len: usize,
    current: Vec<i16>,
    state: State,
}

impl ProbabilityVad {
    pub fn new(vad: &VadConfig, model: &ModelVadConfig) -> Self {
        let frame_len = model.frame_samples.max(1);
        let frame_ms = samples_to_ms(frame_len).max(1);
        let frames = |ms: u32| ms.div_ceil(frame_ms).max(1);
        let pre_roll_len = ms_to_samples(vad.pre_roll_ms);
        Self {
            frame_len,
            start_probability: model.start_probability,
            continue_probability: model.continue_probability.min(model.start_probability),
            min_speech_frames: frames(vad.min_speech_ms),
            end_silence_frames: frames(vad.end_silence_ms),
            max_samples: ms_to_samples(vad.max_utterance_ms).max(frame_len),
            pre_roll: VecDeque::with_capacity(pre_roll_len),
            pre_roll_len,
            current: Vec::new(),
            state: State::Idle,
        }
    }

    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    pub fn in_speech(&self) -> bool {
        matches!(self.state, State::Speech { .. })
    }

    /// Drops any utterance in progress and the pre-roll.
    pub fn reset(&mut self) {
        self.pre_roll.clear();
        self.current.clear();
        self.state = State::Idle;
    }

    /// Ends the stream: an utterance in progress is returned as finished.
    pub fn flush(&mut self) -> Vec<VadEvent> {
        let mut events = Vec::new();
        if let State::Speech { last_voiced, .. } = self.state {
            self.finish(last_voiced, /*truncated*/ false, &mut events);
        }
        self.reset();
        events
    }

    /// One frame of `frame_len` samples and the model's speech probability.
    pub fn push_frame(&mut self, frame: &[i16], probability: f32) -> Vec<VadEvent> {
        let mut events = Vec::new();
        let voiced = probability >= self.continue_probability;
        match self.state {
            State::Idle => {
                if probability >= self.start_probability {
                    self.current.clear();
                    self.current.extend(self.pre_roll.drain(..));
                    self.current.extend_from_slice(frame);
                    self.state = State::Candidate { voiced: 1 };
                    self.confirm_if_ready(&mut events);
                } else {
                    self.push_pre_roll(frame);
                }
            }
            State::Candidate { voiced: count } => {
                self.current.extend_from_slice(frame);
                if voiced {
                    self.state = State::Candidate { voiced: count + 1 };
                    self.confirm_if_ready(&mut events);
                } else {
                    // A blip, not speech: keep its audio as pre-roll.
                    let samples = std::mem::take(&mut self.current);
                    self.push_pre_roll(&samples);
                    self.state = State::Idle;
                }
            }
            State::Speech {
                frames,
                last_voiced,
                unvoiced,
            } => {
                self.current.extend_from_slice(frame);
                let frames = frames + 1;
                let (last_voiced, unvoiced) = if voiced {
                    (frames, 0)
                } else {
                    (last_voiced, unvoiced + 1)
                };
                if unvoiced >= self.end_silence_frames {
                    self.finish(last_voiced, /*truncated*/ false, &mut events);
                } else if self.current.len() >= self.max_samples {
                    self.finish(last_voiced, /*truncated*/ true, &mut events);
                } else {
                    self.state = State::Speech {
                        frames,
                        last_voiced,
                        unvoiced,
                    };
                }
            }
        }
        events
    }

    fn confirm_if_ready(&mut self, events: &mut Vec<VadEvent>) {
        if let State::Candidate { voiced } = self.state
            && voiced >= self.min_speech_frames
        {
            events.push(VadEvent::SpeechStarted);
            self.state = State::Speech {
                frames: voiced,
                last_voiced: voiced,
                unvoiced: 0,
            };
        }
    }

    fn finish(&mut self, last_voiced: u32, truncated: bool, events: &mut Vec<VadEvent>) {
        let samples = std::mem::take(&mut self.current);
        let voiced_ms = samples_to_ms(last_voiced as usize * self.frame_len);
        events.push(VadEvent::SpeechEnded(Utterance {
            samples,
            voiced_ms,
            truncated,
        }));
        self.pre_roll.clear();
        self.state = State::Idle;
    }

    fn push_pre_roll(&mut self, samples: &[i16]) {
        if self.pre_roll_len == 0 {
            return;
        }
        self.pre_roll.extend(samples.iter().copied());
        let excess = self.pre_roll.len().saturating_sub(self.pre_roll_len);
        self.pre_roll.drain(..excess);
    }
}

/// The helper process: one probability per frame written to it.
pub struct VadModel {
    argv: Vec<String>,
    guard: ProcessGuard,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    frame_bytes: Vec<u8>,
}

impl VadModel {
    /// Starts the helper and waits for its `ready` line.
    pub async fn spawn(config: &ModelVadConfig) -> Result<Self> {
        if config.command.is_empty() {
            return Err(VoiceError::Config(
                "no speech detection helper command (see [vad.model] command, or install uv to use the bundled silero_vad.py)"
                    .to_string(),
            ));
        }
        let argv = config.command.clone();
        let mut command = build_command(&argv)?;
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let mut guard = ProcessGuard::spawn(command, &argv)?;
        let child = guard.child_mut();
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(VoiceError::Command(format!(
                "`{}` has no stdin/stdout",
                program_name(&argv)
            )));
        };
        let mut model = Self {
            argv,
            guard,
            stdin,
            lines: BufReader::new(stdout).lines(),
            frame_bytes: Vec::with_capacity(config.frame_samples * 2),
        };
        let timeout = Duration::from_millis(config.startup_timeout_ms.max(1));
        match tokio::time::timeout(timeout, model.next_line()).await {
            Ok(Ok(line)) if line.trim() == "ready" => Ok(model),
            Ok(Ok(line)) => Err(VoiceError::Command(format!(
                "`{}` did not start: expected `ready`, got `{}`",
                program_name(&model.argv),
                line.trim()
            ))),
            Ok(Err(err)) => Err(model.failed(err).await),
            Err(_) => {
                model.guard.kill_and_wait().await;
                Err(VoiceError::Timeout(config.startup_timeout_ms))
            }
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.guard.id()
    }

    /// The speech probability of one frame.
    pub async fn probability(&mut self, frame: &[i16]) -> Result<f32> {
        self.frame_bytes.clear();
        for sample in frame {
            self.frame_bytes.extend_from_slice(&sample.to_le_bytes());
        }
        if let Err(err) = self.stdin.write_all(&self.frame_bytes).await {
            return Err(self.failed(VoiceError::Io(err)).await);
        }
        if let Err(err) = self.stdin.flush().await {
            return Err(self.failed(VoiceError::Io(err)).await);
        }
        let line = match self.next_line().await {
            Ok(line) => line,
            Err(err) => return Err(self.failed(err).await),
        };
        line.trim().parse::<f32>().map_err(|_| {
            VoiceError::Command(format!(
                "`{}` wrote `{}`, not a probability",
                program_name(&self.argv),
                line.trim()
            ))
        })
    }

    pub async fn stop(&mut self) {
        self.guard.kill_and_wait().await;
    }

    async fn next_line(&mut self) -> Result<String> {
        match self.lines.next_line().await? {
            Some(line) => Ok(line),
            None => Err(VoiceError::Command(format!(
                "`{}` exited",
                program_name(&self.argv)
            ))),
        }
    }

    /// Adds the helper's stderr tail to a failure.
    async fn failed(&mut self, err: VoiceError) -> VoiceError {
        self.guard.kill_and_wait().await;
        let tail = self.guard.stderr_tail().await;
        let tail = tail.trim();
        if tail.is_empty() {
            err
        } else {
            VoiceError::Command(format!("{err}: {tail}"))
        }
    }
}

fn ms_to_samples(ms: u32) -> usize {
    (u64::from(ms) * u64::from(SAMPLE_RATE) / 1000) as usize
}

fn samples_to_ms(samples: usize) -> u32 {
    (samples as u64 * 1000 / u64::from(SAMPLE_RATE)) as u32
}

#[cfg(test)]
#[path = "model_vad_tests.rs"]
mod tests;
