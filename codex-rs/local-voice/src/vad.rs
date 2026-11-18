//! Energy-based voice activity detection with an adaptive noise floor.
//!
//! Pure and synchronous: feed 16 kHz mono i16 samples with [`Vad::push`] and
//! get [`VadEvent`]s back.

use std::collections::VecDeque;

use serde::Deserialize;
use serde::Serialize;

pub const SAMPLE_RATE: u32 = 16_000;

/// Frames used to measure the initial noise floor before detecting speech.
const CALIBRATION_FRAMES: u32 = 10;
/// Frames averaged into one level for the steadiness check.
const STEADY_BLOCK: usize = 3;
/// Frames remembered from before speech, for the background estimate.
const IDLE_LEVELS: usize = 15;
/// Per-frame smoothing of the noise floor when the level falls / rises.
const FLOOR_FALL: f32 = 0.2;
const FLOOR_RISE: f32 = 0.02;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VadConfig {
    pub frame_ms: u32,
    /// A frame this far above the noise floor (dB) may start speech.
    pub start_threshold_db: f32,
    /// Speech continues while frames stay this far above the floor (dB).
    pub continue_threshold_db: f32,
    /// Loud audio must last this long before it counts as speech.
    pub min_speech_ms: u32,
    /// Silence that ends an utterance.
    pub end_silence_ms: u32,
    /// Utterances are cut at this length (pre-roll included).
    pub max_utterance_ms: u32,
    /// Audio kept from before speech started.
    pub pre_roll_ms: u32,
    /// Lowest noise floor, as RMS on the i16 scale.
    pub min_noise_floor: f32,
    /// The floor follows a low percentile of the levels over this window, in
    /// every state, including during speech. A steady rise in background
    /// noise then raises the floor, and the utterance ends instead of running
    /// to `max_utterance_ms`. Pauses between words keep speech itself from
    /// raising it.
    pub noise_window_ms: u32,
    /// The percentile (0.0..1.0) of the window taken as background.
    pub noise_percentile: f32,
    /// Speech varies from syllable to syllable; hiss, fans and hum don't.
    /// When this many milliseconds of an utterance stay within
    /// `steady_db` (standard deviation of the frame levels, in dB), the
    /// sound is taken as noise: the floor moves up to it, and an utterance
    /// that was steady from its start is discarded without transcription.
    pub steady_window_ms: u32,
    pub steady_db: f32,
    /// An utterance also ends when the sound falls back to within this many
    /// dB of the background heard just before it started, so hiss that sits
    /// above the floor doesn't keep it open.
    pub end_margin_db: f32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            frame_ms: 20,
            start_threshold_db: 12.0,
            continue_threshold_db: 6.0,
            min_speech_ms: 200,
            end_silence_ms: 700,
            max_utterance_ms: 30_000,
            pre_roll_ms: 300,
            min_noise_floor: 60.0,
            noise_window_ms: 4000,
            noise_percentile: 0.1,
            steady_window_ms: 600,
            steady_db: 2.0,
            end_margin_db: 3.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Utterance {
    /// 16 kHz mono samples: pre-roll, speech and the trailing silence.
    pub samples: Vec<i16>,
    /// Time from the first loud frame to the last one.
    pub voiced_ms: u32,
    /// The utterance hit `max_utterance_ms`.
    pub truncated: bool,
}

impl Utterance {
    pub fn duration_ms(&self) -> u32 {
        samples_to_ms(self.samples.len())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum VadEvent {
    SpeechStarted,
    SpeechEnded(Utterance),
    /// What started as speech was steady noise; nothing to transcribe.
    SpeechDiscarded,
}

#[derive(Debug)]
enum State {
    Idle,
    /// Loud frames seen, not yet `min_speech_ms` of them.
    Candidate {
        loud_frames: u32,
    },
    Speech {
        /// Frames since the first loud frame.
        frames: u32,
        /// Frame count at the last loud frame.
        last_loud_frame: u32,
        silent_frames: u32,
    },
}

#[derive(Debug)]
pub struct Vad {
    config: VadConfig,
    frame_len: usize,
    floor: f32,
    calibration: Vec<f32>,
    /// Recent frame levels, for the background percentile.
    levels: VecDeque<f32>,
    levels_cap: usize,
    /// dB levels of the current utterance, averaged over `STEADY_BLOCK`
    /// frames so per-frame jitter in hiss doesn't hide that it's steady.
    recent_db: VecDeque<f32>,
    steady_frames: usize,
    block_sum: f32,
    block_len: usize,
    /// Levels of the frames just before speech (the background).
    idle_levels: VecDeque<f32>,
    /// The background level heard just before the current utterance.
    utterance_background: f32,
    pending: Vec<i16>,
    pre_roll: VecDeque<i16>,
    pre_roll_len: usize,
    current: Vec<i16>,
    state: State,
}

impl Vad {
    pub fn new(config: VadConfig) -> Self {
        let frame_len = ms_to_samples(config.frame_ms.max(5));
        let pre_roll_len = ms_to_samples(config.pre_roll_ms);
        let floor = config.min_noise_floor.max(1.0);
        let levels_cap = (config.noise_window_ms / config.frame_ms.max(5)).max(1) as usize;
        let steady_frames = (config.steady_window_ms / config.frame_ms.max(5)).max(2) as usize;
        Self {
            config,
            frame_len,
            floor,
            calibration: Vec::new(),
            levels: VecDeque::with_capacity(levels_cap),
            levels_cap,
            recent_db: VecDeque::new(),
            steady_frames: steady_frames.div_ceil(STEADY_BLOCK).max(3),
            block_sum: 0.0,
            block_len: 0,
            idle_levels: VecDeque::with_capacity(IDLE_LEVELS),
            utterance_background: 0.0,
            pending: Vec::new(),
            pre_roll: VecDeque::with_capacity(pre_roll_len),
            pre_roll_len,
            current: Vec::new(),
            state: State::Idle,
        }
    }

    pub fn noise_floor(&self) -> f32 {
        self.floor
    }

    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    pub fn in_speech(&self) -> bool {
        matches!(self.state, State::Speech { .. })
    }

    /// Starts detecting at once, keeping the current floor (used when audio
    /// arrives that is already known to be speech, e.g. a barge-in).
    pub fn end_calibration(&mut self) {
        let missing =
            CALIBRATION_FRAMES as usize - self.calibration.len().min(CALIBRATION_FRAMES as usize);
        self.calibration
            .extend(std::iter::repeat_n(self.floor, missing));
    }

    /// Drops any utterance in progress and the pre-roll; keeps the floor.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.pre_roll.clear();
        self.current.clear();
        self.recent_db.clear();
        self.state = State::Idle;
    }

    pub fn push(&mut self, samples: &[i16]) -> Vec<VadEvent> {
        let mut events = Vec::new();
        self.pending.extend_from_slice(samples);
        let mut offset = 0;
        while self.pending.len() - offset >= self.frame_len {
            let frame: Vec<i16> = self.pending[offset..offset + self.frame_len].to_vec();
            offset += self.frame_len;
            self.process_frame(&frame, &mut events);
        }
        self.pending.drain(..offset);
        events
    }

    /// Ends the stream: an utterance in progress is returned as finished.
    pub fn flush(&mut self) -> Vec<VadEvent> {
        let mut events = Vec::new();
        if let State::Speech {
            last_loud_frame, ..
        } = self.state
        {
            let mut samples = std::mem::take(&mut self.current);
            samples.extend(self.pending.drain(..));
            events.push(VadEvent::SpeechEnded(Utterance {
                samples,
                voiced_ms: self.frames_to_ms(last_loud_frame),
                truncated: false,
            }));
        }
        self.reset();
        events
    }

    fn process_frame(&mut self, frame: &[i16], events: &mut Vec<VadEvent>) {
        let level = rms(frame);
        self.track_background(level);
        tracing::trace!(
            level_db = ratio_to_db(level),
            floor_db = ratio_to_db(self.floor),
            state = ?self.state,
            "vad frame"
        );
        if self.calibration.len() < CALIBRATION_FRAMES as usize {
            if level < 1.0 {
                // Digital silence (a device warming up) says nothing about
                // the room; calibrate on the first frames with any signal.
                self.push_pre_roll(frame);
                return;
            }
            self.calibration.push(level);
            let mean = self.calibration.iter().sum::<f32>() / self.calibration.len() as f32;
            self.floor = mean.max(self.config.min_noise_floor.max(1.0));
            self.push_pre_roll(frame);
            return;
        }
        let start = self.floor * db_to_ratio(self.config.start_threshold_db);
        let keep = self.floor * db_to_ratio(self.config.continue_threshold_db);
        let min_speech_frames = self.ms_to_frames(self.config.min_speech_ms).max(1);
        let end_silence_frames = self.ms_to_frames(self.config.end_silence_ms).max(1);
        let max_samples = ms_to_samples(self.config.max_utterance_ms).max(self.frame_len);

        match self.state {
            State::Idle => {
                if self.idle_levels.len() == IDLE_LEVELS {
                    self.idle_levels.pop_front();
                }
                self.idle_levels.push_back(level);
                if level >= start {
                    self.current = self.pre_roll.drain(..).collect();
                    self.current.extend_from_slice(frame);
                    self.state = State::Candidate { loud_frames: 1 };
                    self.confirm_if_ready(min_speech_frames, events);
                } else {
                    self.adapt_floor(level);
                    self.push_pre_roll(frame);
                }
            }
            State::Candidate { loud_frames } => {
                if level >= keep {
                    self.current.extend_from_slice(frame);
                    self.state = State::Candidate {
                        loud_frames: loud_frames + 1,
                    };
                    self.confirm_if_ready(min_speech_frames, events);
                } else {
                    // Too short: a click or a bump. Back to idle, keeping the
                    // audio as pre-roll.
                    let current = std::mem::take(&mut self.current);
                    self.state = State::Idle;
                    self.push_pre_roll(&current);
                    self.adapt_floor(level);
                    self.push_pre_roll(frame);
                }
            }
            State::Speech {
                frames,
                last_loud_frame,
                silent_frames,
            } => {
                self.current.extend_from_slice(frame);
                let frames = frames + 1;
                if let Some(steady_level) = self.note_speech_level(level) {
                    let since_start = frames as usize;
                    // Move the floor up to the steady sound: frames at that
                    // level no longer count as loud, so a later utterance
                    // must stand out from it.
                    self.raise_floor(steady_level);
                    if since_start
                        <= self.steady_frames * STEADY_BLOCK
                            + min_speech_frames as usize
                            + STEADY_BLOCK
                    {
                        tracing::debug!(
                            level_db = ratio_to_db(steady_level),
                            "vad: steady sound from the start of the utterance; discarding it as noise"
                        );
                        self.discard(events);
                        return;
                    }
                }
                let keep = (self.floor * db_to_ratio(self.config.continue_threshold_db))
                    .max(self.utterance_background * db_to_ratio(self.config.end_margin_db));
                let (last_loud_frame, silent_frames) = if level >= keep {
                    (frames, 0)
                } else {
                    (last_loud_frame, silent_frames + 1)
                };
                if self.current.len() >= max_samples {
                    self.finish(last_loud_frame, true, events);
                } else if silent_frames >= end_silence_frames {
                    self.finish(last_loud_frame, false, events);
                } else {
                    self.state = State::Speech {
                        frames,
                        last_loud_frame,
                        silent_frames,
                    };
                }
            }
        }
    }

    fn confirm_if_ready(&mut self, min_speech_frames: u32, events: &mut Vec<VadEvent>) {
        if let State::Candidate { loud_frames } = self.state
            && loud_frames >= min_speech_frames
        {
            self.state = State::Speech {
                frames: loud_frames,
                last_loud_frame: loud_frames,
                silent_frames: 0,
            };
            self.utterance_background = median(&self.idle_levels).unwrap_or(0.0);
            events.push(VadEvent::SpeechStarted);
        }
    }

    /// Records a frame level of the utterance in progress. Returns the mean
    /// level when the last `steady_window_ms` stayed within `steady_db`.
    fn note_speech_level(&mut self, level: f32) -> Option<f32> {
        self.block_sum += level;
        self.block_len += 1;
        if self.block_len < STEADY_BLOCK {
            return None;
        }
        let block = self.block_sum / self.block_len as f32;
        self.block_sum = 0.0;
        self.block_len = 0;
        if self.recent_db.len() == self.steady_frames {
            self.recent_db.pop_front();
        }
        self.recent_db.push_back(ratio_to_db(block));
        if self.recent_db.len() < self.steady_frames {
            return None;
        }
        let n = self.recent_db.len() as f32;
        let mean = self.recent_db.iter().sum::<f32>() / n;
        let variance = self
            .recent_db
            .iter()
            .map(|db| (db - mean).powi(2))
            .sum::<f32>()
            / n;
        (variance.sqrt() < self.config.steady_db).then(|| {
            self.recent_db.clear();
            db_from_dbfs(mean)
        })
    }

    fn raise_floor(&mut self, floor: f32) {
        if floor > self.floor {
            tracing::debug!(
                from_db = ratio_to_db(self.floor),
                to_db = ratio_to_db(floor),
                "vad: steady sound, raising the floor"
            );
            self.floor = floor;
        }
    }

    fn discard(&mut self, events: &mut Vec<VadEvent>) {
        self.current.clear();
        self.pre_roll.clear();
        self.recent_db.clear();
        self.block_sum = 0.0;
        self.block_len = 0;
        self.state = State::Idle;
        events.push(VadEvent::SpeechDiscarded);
    }

    fn finish(&mut self, last_loud_frame: u32, truncated: bool, events: &mut Vec<VadEvent>) {
        let samples = std::mem::take(&mut self.current);
        self.state = State::Idle;
        self.pre_roll.clear();
        self.recent_db.clear();
        self.block_sum = 0.0;
        self.block_len = 0;
        // The background before this utterance is the room; don't let the
        // floor sit below it, or the same hiss starts the next one.
        let background = self.utterance_background;
        self.raise_floor(background);
        events.push(VadEvent::SpeechEnded(Utterance {
            samples,
            voiced_ms: self.frames_to_ms(last_loud_frame),
            truncated,
        }));
    }

    /// Records `level` and raises the floor to the window's low percentile
    /// once the window is full and the current frame is at that background.
    /// Only raises: falling is `adapt_floor`'s job.
    fn track_background(&mut self, level: f32) {
        if self.levels.len() == self.levels_cap {
            self.levels.pop_front();
        }
        self.levels.push_back(level);
        if self.levels.len() < self.levels_cap {
            return;
        }
        let mut sorted: Vec<f32> = self.levels.iter().copied().collect();
        let percentile = self.config.noise_percentile.clamp(0.0, 1.0);
        let index = ((sorted.len() - 1) as f32 * percentile).round() as usize;
        let (_, background, _) = sorted.select_nth_unstable_by(index, f32::total_cmp);
        // Raise only while the noise is still there: when it stops, the window
        // is still full of loud frames, and raising then would deafen the VAD
        // for a whole window. `adapt_floor` brings the floor back down.
        if *background > self.floor && level >= *background * 0.5 {
            tracing::debug!(
                from_db = ratio_to_db(self.floor),
                to_db = ratio_to_db(*background),
                "vad: background noise rose, raising the floor"
            );
            self.floor = *background;
        }
    }

    fn adapt_floor(&mut self, level: f32) {
        let rate = if level < self.floor {
            FLOOR_FALL
        } else {
            FLOOR_RISE
        };
        self.floor += (level - self.floor) * rate;
        self.floor = self.floor.max(self.config.min_noise_floor.max(1.0));
    }

    fn push_pre_roll(&mut self, samples: &[i16]) {
        if self.pre_roll_len == 0 {
            return;
        }
        self.pre_roll.extend(samples.iter().copied());
        while self.pre_roll.len() > self.pre_roll_len {
            self.pre_roll.pop_front();
        }
    }

    fn ms_to_frames(&self, ms: u32) -> u32 {
        ms.div_ceil(self.config.frame_ms.max(5))
    }

    fn frames_to_ms(&self, frames: u32) -> u32 {
        samples_to_ms(frames as usize * self.frame_len)
    }
}

/// Detects sustained loud speech while the assistant is talking.
#[derive(Debug)]
pub struct BargeInDetector {
    ratio: f32,
    min_frames: u32,
    frame_len: usize,
    run: u32,
    pending: Vec<i16>,
    buffered: Vec<i16>,
}

impl BargeInDetector {
    pub fn new(threshold_db: f32, min_ms: u32, frame_len: usize) -> Self {
        let frame_ms = samples_to_ms(frame_len).max(1);
        Self {
            ratio: db_to_ratio(threshold_db),
            min_frames: min_ms.div_ceil(frame_ms).max(1),
            frame_len: frame_len.max(1),
            run: 0,
            pending: Vec::new(),
            buffered: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.run = 0;
        self.pending.clear();
        self.buffered.clear();
    }

    /// Returns the loud audio once the level has been held long enough.
    pub fn push(&mut self, samples: &[i16], floor: f32) -> Option<Vec<i16>> {
        self.pending.extend_from_slice(samples);
        let threshold = floor * self.ratio;
        let mut offset = 0;
        let mut triggered = false;
        while self.pending.len() - offset >= self.frame_len {
            let frame = &self.pending[offset..offset + self.frame_len];
            offset += self.frame_len;
            if triggered {
                self.buffered.extend_from_slice(frame);
                continue;
            }
            if rms(frame) >= threshold {
                self.run += 1;
                self.buffered.extend_from_slice(frame);
                if self.run >= self.min_frames {
                    triggered = true;
                }
            } else {
                self.run = 0;
                self.buffered.clear();
            }
        }
        if triggered {
            let mut audio = std::mem::take(&mut self.buffered);
            audio.extend(self.pending.drain(offset..));
            self.reset();
            return Some(audio);
        }
        self.pending.drain(..offset);
        None
    }
}

pub fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|sample| {
            let value = f64::from(*sample);
            value * value
        })
        .sum();
    (sum / samples.len() as f64).sqrt() as f32
}

fn median(levels: &VecDeque<f32>) -> Option<f32> {
    if levels.is_empty() {
        return None;
    }
    let mut sorted: Vec<f32> = levels.iter().copied().collect();
    let middle = sorted.len() / 2;
    let (_, value, _) = sorted.select_nth_unstable_by(middle, f32::total_cmp);
    Some(*value)
}

/// The inverse of [`ratio_to_db`]: dBFS back to an i16-scale RMS level.
fn db_from_dbfs(db: f32) -> f32 {
    f32::from(i16::MAX) * 10f32.powf(db / 20.0)
}

/// An i16-scale RMS level as dBFS, for logs.
pub fn ratio_to_db(level: f32) -> f32 {
    20.0 * (level.max(1e-3) / f32::from(i16::MAX)).log10()
}

pub fn db_to_ratio(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn ms_to_samples(ms: u32) -> usize {
    (u64::from(ms) * u64::from(SAMPLE_RATE) / 1000) as usize
}

pub fn samples_to_ms(samples: usize) -> u32 {
    (samples as u64 * 1000 / u64::from(SAMPLE_RATE)) as u32
}

#[cfg(test)]
#[path = "vad_tests.rs"]
mod tests;
