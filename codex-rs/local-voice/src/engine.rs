//! The running engine: capture → VAD → STT worker, and a TTS speaker queue,
//! joined by half-duplex echo suppression.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::capture::Capture;
use crate::config::LocalVoiceConfig;
use crate::config::TranscriptFilterConfig;
use crate::error::VoiceError;
use crate::error::VoiceStage;
use crate::process::run_cancellable;
use crate::stt::SpeechToText;
use crate::stt::build_stt;
use crate::template::substitute_argv;
use crate::transcript_filter::clean_transcript;
use crate::tts::Speech;
use crate::tts::TextToSpeech;
use crate::tts::build_tts;
use crate::tts::prepare_text;
use crate::vad::BargeInDetector;
use crate::vad::SAMPLE_RATE;
use crate::vad::Utterance;
use crate::vad::Vad;
use crate::vad::VadEvent;
use crate::wav::write_wav;

const EVENT_CAPACITY: usize = 256;

#[derive(Clone, Debug, PartialEq)]
pub enum LocalVoiceEvent {
    /// The microphone is open and the engine waits for speech.
    Listening,
    SpeechStarted,
    /// The utterance ended and is being transcribed.
    SpeechEnded,
    /// A final, non-empty transcript: send it as a user turn.
    Transcript {
        text: String,
    },
    SpeakingStarted,
    SpeakingFinished,
    Error {
        stage: VoiceStage,
        message: String,
    },
}

pub struct LocalVoice;

impl LocalVoice {
    /// Starts the engine on the current tokio runtime. Problems (a missing
    /// program, a dead capture command, a misconfigured MCP tool) arrive as
    /// `Error` events; this never fails.
    pub fn start(config: LocalVoiceConfig) -> (LocalVoiceHandle, mpsc::Receiver<LocalVoiceEvent>) {
        let (events_tx, events_rx) = mpsc::channel(EVENT_CAPACITY);
        let (speak_tx, speak_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _) = watch::channel(false);
        let (generation_tx, _) = watch::channel(0u64);
        let workdir = tempfile::Builder::new()
            .prefix("codex-local-voice-")
            .tempdir()
            .map_err(|err| format!("cannot create a private temp dir: {err}"));
        let shared = Arc::new(Shared {
            events: events_tx,
            muted: AtomicBool::new(false),
            playback: Mutex::new(PlaybackState::default()),
            generation: generation_tx,
            shutdown: shutdown_tx,
            capture_pid: AtomicU32::new(0),
            echo_tail: Duration::from_millis(config.playback.echo_tail_ms),
        });
        let inner = Arc::new(Inner {
            shared: Arc::clone(&shared),
            speak_tx,
            max_chars: config.playback.max_chars,
            tasks: Mutex::new(Vec::new()),
            workdir: Mutex::new(None),
        });
        let handle = LocalVoiceHandle { inner };

        let workdir = match workdir {
            Ok(dir) => dir,
            Err(message) => {
                shared.emit_now(LocalVoiceEvent::Error {
                    stage: VoiceStage::Config,
                    message,
                });
                return (handle, events_rx);
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(workdir.path(), std::fs::Permissions::from_mode(0o700));
        }
        let workdir_path = workdir.path().to_path_buf();
        *handle
            .inner
            .workdir
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(workdir);

        let mut tasks = Vec::new();
        match build_tts(&config.tts) {
            Ok(tts) => tasks.push(tokio::spawn(speaker_loop(
                Arc::clone(&shared),
                tts,
                config.playback.player.clone(),
                speak_rx,
                workdir_path.clone(),
            ))),
            Err(err) => shared.emit_now(LocalVoiceEvent::Error {
                stage: VoiceStage::Config,
                message: format!("tts: {err}"),
            }),
        }
        if config.capture.enabled {
            match build_stt(&config.stt) {
                Ok(stt) => {
                    let (utterance_tx, utterance_rx) = mpsc::unbounded_channel();
                    tasks.push(tokio::spawn(stt_loop(
                        Arc::clone(&shared),
                        stt,
                        config.transcripts.clone(),
                        utterance_rx,
                        workdir_path,
                    )));
                    tasks.push(tokio::spawn(audio_loop(
                        Arc::clone(&shared),
                        config,
                        utterance_tx,
                    )));
                }
                Err(err) => shared.emit_now(LocalVoiceEvent::Error {
                    stage: VoiceStage::Config,
                    message: format!("stt: {err}"),
                }),
            }
        }
        *handle
            .inner
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = tasks;
        (handle, events_rx)
    }
}

/// Controls a running engine. Cloning shares the engine; dropping the last
/// clone shuts it down (children are killed).
#[derive(Clone)]
pub struct LocalVoiceHandle {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Arc<Shared>,
    speak_tx: mpsc::UnboundedSender<SpeakRequest>,
    max_chars: usize,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    workdir: Mutex<Option<tempfile::TempDir>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.shutdown.send_replace(true);
        self.shared
            .generation
            .send_modify(|generation| *generation += 1);
    }
}

impl LocalVoiceHandle {
    /// Queues `text` to be spoken after anything already queued. Text is
    /// trimmed and capped at `playback.max_chars`.
    pub fn speak(&self, text: impl AsRef<str>) {
        let Some(text) = prepare_text(text.as_ref(), self.inner.max_chars) else {
            return;
        };
        let generation = *self.inner.shared.generation.borrow();
        let _ = self.inner.speak_tx.send(SpeakRequest { text, generation });
    }

    /// Stops the current speech (killing the player) and drops the queue.
    pub fn stop_speaking(&self) {
        self.inner.shared.stop_speaking();
    }

    /// While muted, microphone audio is discarded. Unmuting emits
    /// `Listening`.
    pub fn set_muted(&self, muted: bool) {
        let was = self.inner.shared.muted.swap(muted, Ordering::SeqCst);
        if was && !muted && self.inner.shared.capture_pid.load(Ordering::SeqCst) != 0 {
            self.inner.shared.emit_now(LocalVoiceEvent::Listening);
        }
    }

    pub fn is_muted(&self) -> bool {
        self.inner.shared.muted.load(Ordering::SeqCst)
    }

    pub fn is_speaking(&self) -> bool {
        self.inner.shared.playback_state().speaking
    }

    /// The capture command's pid while it runs.
    pub fn capture_pid(&self) -> Option<u32> {
        match self.inner.shared.capture_pid.load(Ordering::SeqCst) {
            0 => None,
            pid => Some(pid),
        }
    }

    /// Stops everything: kills and reaps the capture, STT and player
    /// children, then removes the temp dir.
    pub async fn shutdown(&self) {
        self.inner.shared.shutdown.send_replace(true);
        self.inner.shared.stop_speaking();
        let tasks = std::mem::take(
            &mut *self
                .inner
                .tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for task in tasks {
            if tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .is_err()
            {
                tracing::warn!("local voice task did not stop in time");
            }
        }
        let workdir = self
            .inner
            .workdir
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        drop(workdir);
    }
}

struct SpeakRequest {
    text: String,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct PlaybackState {
    speaking: bool,
    quiet_since: Option<Instant>,
    /// The user barged in: no echo tail after the interrupted speech.
    barged_in: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Echo {
    Open,
    Speaking,
    Tail,
}

struct Shared {
    events: mpsc::Sender<LocalVoiceEvent>,
    muted: AtomicBool,
    playback: Mutex<PlaybackState>,
    /// Bumped by `stop_speaking`; requests from older generations are
    /// dropped and the current one is interrupted.
    generation: watch::Sender<u64>,
    shutdown: watch::Sender<bool>,
    capture_pid: AtomicU32,
    echo_tail: Duration,
}

impl Shared {
    async fn emit(&self, event: LocalVoiceEvent) {
        let _ = self.events.send(event).await;
    }

    fn emit_now(&self, event: LocalVoiceEvent) {
        if let Err(err) = self.events.try_send(event) {
            tracing::warn!("local voice event dropped: {err}");
        }
    }

    async fn error(&self, stage: VoiceStage, err: impl std::fmt::Display) {
        let message = err.to_string();
        tracing::warn!(%stage, "{message}");
        self.emit(LocalVoiceEvent::Error { stage, message }).await;
    }

    fn stop_speaking(&self) {
        self.generation.send_modify(|generation| *generation += 1);
    }

    fn playback_state(&self) -> PlaybackState {
        *self.playback.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update_playback(&self, update: impl FnOnce(&mut PlaybackState)) {
        update(&mut self.playback.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn set_speaking(&self, speaking: bool) {
        self.update_playback(|state| {
            state.speaking = speaking;
            if speaking {
                state.barged_in = false;
                state.quiet_since = None;
            } else {
                state.quiet_since = Some(Instant::now());
            }
        });
    }

    fn echo(&self, now: Instant) -> Echo {
        let state = self.playback_state();
        if state.barged_in {
            return Echo::Open;
        }
        if state.speaking {
            return Echo::Speaking;
        }
        match state.quiet_since {
            Some(since) if now.duration_since(since) < self.echo_tail => Echo::Tail,
            _ => Echo::Open,
        }
    }

    fn cancel_token(&self, generation: u64) -> Cancel {
        Cancel {
            generation: self.generation.subscribe(),
            expected: generation,
            shutdown: self.shutdown.subscribe(),
        }
    }
}

/// Resolves when `stop_speaking` or shutdown happens.
struct Cancel {
    generation: watch::Receiver<u64>,
    expected: u64,
    shutdown: watch::Receiver<bool>,
}

impl Cancel {
    fn is_cancelled(&self) -> bool {
        *self.generation.borrow() != self.expected || *self.shutdown.borrow()
    }

    async fn wait(&mut self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            tokio::select! {
                changed = self.generation.changed() => if changed.is_err() { return },
                changed = self.shutdown.changed() => if changed.is_err() { return },
            }
        }
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn audio_loop(
    shared: Arc<Shared>,
    config: LocalVoiceConfig,
    utterances: mpsc::UnboundedSender<Utterance>,
) {
    let mut shutdown = shared.shutdown.subscribe();
    let mut capture = match Capture::spawn(&config.capture) {
        Ok(capture) => capture,
        Err(err) => {
            shared.error(VoiceStage::Capture, err).await;
            return;
        }
    };
    shared
        .capture_pid
        .store(capture.pid().unwrap_or(0), Ordering::SeqCst);
    shared.emit(LocalVoiceEvent::Listening).await;

    let mut vad = Vad::new(config.vad.clone());
    let mut barge_in = BargeInDetector::new(
        config.playback.barge_in_threshold_db,
        config.playback.barge_in_min_ms,
        vad.frame_len(),
    );
    loop {
        let samples = tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => {
                capture.stop().await;
                shared.capture_pid.store(0, Ordering::SeqCst);
                return;
            }
            samples = capture.next_samples() => samples,
        };
        let Some(samples) = samples else {
            for event in vad.flush() {
                handle_vad_event(&shared, &utterances, event).await;
            }
            let message = capture.exit_message().await;
            shared.capture_pid.store(0, Ordering::SeqCst);
            shared.error(VoiceStage::Capture, message).await;
            return;
        };
        if shared.muted.load(Ordering::SeqCst) {
            vad.reset();
            barge_in.reset();
            continue;
        }
        match shared.echo(Instant::now()) {
            Echo::Speaking if config.playback.barge_in => {
                if let Some(audio) = barge_in.push(&samples, vad.noise_floor()) {
                    tracing::debug!("barge-in: stopping playback");
                    shared.update_playback(|state| state.barged_in = true);
                    shared.stop_speaking();
                    vad.reset();
                    vad.end_calibration();
                    for event in vad.push(&audio) {
                        handle_vad_event(&shared, &utterances, event).await;
                    }
                }
                continue;
            }
            Echo::Speaking | Echo::Tail => {
                // Half duplex: never transcribe our own voice.
                vad.reset();
                barge_in.reset();
                continue;
            }
            Echo::Open => {}
        }
        for event in vad.push(&samples) {
            handle_vad_event(&shared, &utterances, event).await;
        }
    }
}

async fn handle_vad_event(
    shared: &Shared,
    utterances: &mpsc::UnboundedSender<Utterance>,
    event: VadEvent,
) {
    match event {
        VadEvent::SpeechStarted => shared.emit(LocalVoiceEvent::SpeechStarted).await,
        VadEvent::SpeechEnded(utterance) => {
            shared.emit(LocalVoiceEvent::SpeechEnded).await;
            let _ = utterances.send(utterance);
        }
    }
}

async fn stt_loop(
    shared: Arc<Shared>,
    stt: Arc<dyn SpeechToText>,
    filter: TranscriptFilterConfig,
    mut utterances: mpsc::UnboundedReceiver<Utterance>,
    workdir: PathBuf,
) {
    let mut shutdown = shared.shutdown.subscribe();
    tokio::select! {
        () = wait_for_shutdown(&mut shutdown) => return,
        prepared = stt.prepare() => if let Err(err) = prepared {
            shared.error(VoiceStage::Stt, err).await;
        },
    }
    let mut count = 0u64;
    loop {
        let utterance = tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => return,
            utterance = utterances.recv() => match utterance {
                Some(utterance) => utterance,
                None => return,
            },
        };
        count += 1;
        let scratch = workdir.join(format!("utterance-{count}"));
        let result = tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => {
                let _ = tokio::fs::remove_dir_all(&scratch).await;
                return;
            }
            result = transcribe(stt.as_ref(), &utterance, &scratch) => result,
        };
        let _ = tokio::fs::remove_dir_all(&scratch).await;
        match result {
            Ok(text) => match clean_transcript(&text, utterance.voiced_ms, &filter) {
                Some(text) => shared.emit(LocalVoiceEvent::Transcript { text }).await,
                None => tracing::debug!(transcript = %text, "dropped empty or noise transcript"),
            },
            Err(err) => shared.error(VoiceStage::Stt, err).await,
        }
        shared.emit(LocalVoiceEvent::Listening).await;
    }
}

async fn transcribe(
    stt: &dyn SpeechToText,
    utterance: &Utterance,
    scratch: &Path,
) -> crate::Result<String> {
    tokio::fs::create_dir_all(scratch).await?;
    let wav = scratch.join("utterance.wav");
    write_wav(&wav, &utterance.samples, SAMPLE_RATE).await?;
    stt.transcribe(&wav, scratch).await
}

async fn speaker_loop(
    shared: Arc<Shared>,
    tts: Arc<dyn TextToSpeech>,
    player: Vec<String>,
    mut requests: mpsc::UnboundedReceiver<SpeakRequest>,
    workdir: PathBuf,
) {
    let mut shutdown = shared.shutdown.subscribe();
    tokio::select! {
        () = wait_for_shutdown(&mut shutdown) => return,
        prepared = tts.prepare() => if let Err(err) = prepared {
            shared.error(VoiceStage::Tts, err).await;
        },
    }
    let mut count = 0u64;
    loop {
        let request = tokio::select! {
            () = wait_for_shutdown(&mut shutdown) => return,
            request = requests.recv() => match request {
                Some(request) => request,
                None => return,
            },
        };
        if request.generation != *shared.generation.borrow() {
            continue;
        }
        count += 1;
        let out_wav = workdir.join(format!("speech-{count}.wav"));
        let mut cancel = shared.cancel_token(request.generation);
        let result = speak_one(
            &shared,
            tts.as_ref(),
            &player,
            &request.text,
            &out_wav,
            &mut cancel,
        )
        .await;
        let _ = tokio::fs::remove_file(&out_wav).await;
        if let Err((stage, err)) = result {
            shared.error(stage, err).await;
        }
        if *shutdown.borrow() {
            return;
        }
    }
}

async fn speak_one(
    shared: &Shared,
    tts: &dyn TextToSpeech,
    player: &[String],
    text: &str,
    out_wav: &Path,
    cancel: &mut Cancel,
) -> Result<(), (VoiceStage, VoiceError)> {
    let speech = tokio::select! {
        speech = tts.synthesize(text, out_wav) => speech.map_err(|err| (VoiceStage::Tts, err))?,
        () = cancel.wait() => return Ok(()),
    };
    let (argv, cleanup) = match speech {
        Speech::Command(argv) => (argv, None),
        Speech::Wav { path, delete_after } => {
            let wav = path.to_string_lossy().into_owned();
            let argv = substitute_argv(player, &[("wav", wav.as_str())]);
            (argv, delete_after.then_some(path))
        }
    };
    if cancel.is_cancelled() {
        remove_quietly(cleanup.as_deref()).await;
        return Ok(());
    }
    shared.set_speaking(true);
    shared.emit(LocalVoiceEvent::SpeakingStarted).await;
    let played = run_cancellable(&argv, cancel.wait(), |pid| {
        tracing::debug!(?pid, "speaking");
    })
    .await;
    shared.set_speaking(false);
    remove_quietly(cleanup.as_deref()).await;
    shared.emit(LocalVoiceEvent::SpeakingFinished).await;
    match played {
        Ok(()) | Err(VoiceError::Interrupted) => Ok(()),
        Err(err) => Err((VoiceStage::Playback, err)),
    }
}

async fn remove_quietly(path: Option<&Path>) {
    if let Some(path) = path {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
