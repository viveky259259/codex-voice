use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use pretty_assertions::assert_eq;

use super::*;
use crate::config::CaptureConfig;
use crate::config::CommandSttConfig;
use crate::config::CommandToFileTtsConfig;
use crate::config::CommandTtsConfig;
use crate::config::PlaybackConfig;
use crate::config::SttConfig;
use crate::config::TtsConfig;
use crate::test_support::path_str;
use crate::test_support::pcm_bytes;
use crate::test_support::pid_gone;
use crate::test_support::read_argv;
use crate::test_support::speech_burst;
use crate::test_support::tone;
use crate::test_support::wait_for;
use crate::test_support::wait_for_file;
use crate::test_support::wait_pid_gone;
use crate::test_support::write_script;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn write_raw(&self, name: &str, samples: &[i16]) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, pcm_bytes(samples)).unwrap();
        path
    }

    fn script(&self, name: &str, body: &str) -> String {
        path_str(&write_script(self.dir.path(), name, body))
    }

    /// A capture command that waits for each `go` file, then writes the
    /// matching raw fixture, then idles.
    fn staged_capture(&self, stages: &[(&str, &Path)]) -> CaptureConfig {
        let mut body = String::new();
        for (go, raw) in stages {
            let go = self.path(go);
            body.push_str(&format!(
                "while [ ! -e '{}' ]; do sleep 0.02; done\ncat '{}'\n",
                go.display(),
                raw.display()
            ));
        }
        body.push_str("exec sleep 60");
        CaptureConfig {
            enabled: true,
            command: vec![self.script("fake-mic", &body)],
            device: String::new(),
        }
    }

    /// STT that records the wav it was given, keeps a copy and prints `text`.
    fn stt(&self, text: &str) -> SttConfig {
        let body = format!(
            "echo \"$1\" > '{}'\ncp \"$1\" '{}'\necho '{text}'",
            self.path("stt-wav-path").display(),
            self.path("stt-copy.wav").display()
        );
        SttConfig::Command(CommandSttConfig {
            command: vec![self.script("fake-stt", &body), "{wav}".to_string()],
            timeout_ms: 10_000,
        })
    }

    /// TTS that records its argv and pid, appends the text to a log, then
    /// "speaks" for `seconds`.
    fn speaker(&self, seconds: u32) -> TtsConfig {
        let body = format!(
            "printf '%s\\0' \"$@\" > '{argv}'\nprintf '%s\\n' \"$1\" >> '{log}'\necho $$ > '{pid}'\nexec sleep {seconds}",
            argv = self.path("tts-argv").display(),
            log = self.path("tts-log").display(),
            pid = self.path("tts-pid").display(),
        );
        TtsConfig::Command(CommandTtsConfig {
            command: vec![self.script("fake-say", &body), "{text}".to_string()],
        })
    }
}

fn config(capture: CaptureConfig, stt: SttConfig, tts: TtsConfig) -> LocalVoiceConfig {
    LocalVoiceConfig {
        capture,
        stt,
        tts,
        ..LocalVoiceConfig::default()
    }
}

fn no_capture() -> CaptureConfig {
    CaptureConfig {
        enabled: false,
        ..CaptureConfig::default()
    }
}

fn is(expected: LocalVoiceEvent) -> impl Fn(&LocalVoiceEvent) -> bool {
    move |event| *event == expected
}

fn is_transcript(event: &LocalVoiceEvent) -> bool {
    matches!(event, LocalVoiceEvent::Transcript { .. })
}

fn drain(events: &mut mpsc::Receiver<LocalVoiceEvent>) -> Vec<LocalVoiceEvent> {
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    seen
}

async fn read_pid(path: &Path) -> u32 {
    wait_for_file(path).await.trim().parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_becomes_a_transcript_and_the_wav_is_deleted() {
    let fx = Fixture::new();
    let raw = fx.write_raw("burst.raw", &speech_burst());
    std::fs::write(fx.path("go"), "").unwrap();
    let (voice, mut events) = LocalVoice::start(config(
        fx.staged_capture(&[("go", &raw)]),
        fx.stt("  hello   world "),
        fx.speaker(0),
    ));
    let seen = wait_for(&mut events, is_transcript).await;
    assert_eq!(
        seen,
        vec![
            LocalVoiceEvent::Listening,
            LocalVoiceEvent::SpeechStarted,
            LocalVoiceEvent::SpeechEnded,
            LocalVoiceEvent::Transcript {
                text: "hello world".to_string()
            },
        ]
    );
    wait_for(&mut events, is(LocalVoiceEvent::Listening)).await;

    // The STT command got a 16 kHz mono wav that is gone afterwards.
    let wav_path = std::fs::read_to_string(fx.path("stt-wav-path")).unwrap();
    assert!(!Path::new(wav_path.trim()).exists());
    let copy = std::fs::read(fx.path("stt-copy.wav")).unwrap();
    assert_eq!(&copy[0..4], b"RIFF");
    assert_eq!(
        u32::from_le_bytes([copy[24], copy[25], copy[26], copy[27]]),
        16_000
    );
    // 300 ms pre-roll + 1 s of speech + 700 ms of trailing silence.
    assert_eq!(copy.len(), 44 + 2 * 16 * 2000);
    voice.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noise_transcripts_are_dropped() {
    let fx = Fixture::new();
    let raw = fx.write_raw("burst.raw", &speech_burst());
    std::fs::write(fx.path("go"), "").unwrap();
    let (voice, mut events) = LocalVoice::start(config(
        fx.staged_capture(&[("go", &raw)]),
        fx.stt("[BLANK_AUDIO]"),
        fx.speaker(0),
    ));
    let seen = wait_for(&mut events, is(LocalVoiceEvent::SpeechEnded)).await;
    assert_eq!(seen.len(), 3);
    wait_for(&mut events, is(LocalVoiceEvent::Listening)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(drain(&mut events), Vec::new());
    voice.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_capture_command_reports_its_stderr() {
    let fx = Fixture::new();
    let capture = CaptureConfig {
        enabled: true,
        command: vec![fx.script(
            "fake-ffmpeg",
            "echo '[avfoundation] Selected audio device index 0 is out of range' >&2\nexit 1",
        )],
        device: String::new(),
    };
    let (voice, mut events) = LocalVoice::start(config(capture, fx.stt("x"), fx.speaker(0)));
    let seen = wait_for(&mut events, |event| {
        matches!(event, LocalVoiceEvent::Error { .. })
    })
    .await;
    let LocalVoiceEvent::Error { stage, message } = seen.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(*stage, VoiceStage::Capture);
    assert!(message.contains("out of range"), "{message}");
    assert!(message.contains("exit status: 1"), "{message}");
    voice.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_while_speaking_is_dropped() {
    let fx = Fixture::new();
    let raw = fx.write_raw("burst.raw", &speech_burst());
    let (voice, mut events) = LocalVoice::start(config(
        fx.staged_capture(&[("go1", &raw), ("go2", &raw)]),
        fx.stt("second burst"),
        fx.speaker(30),
    ));
    wait_for(&mut events, is(LocalVoiceEvent::Listening)).await;
    voice.speak("Running the tests now.");
    wait_for(&mut events, is(LocalVoiceEvent::SpeakingStarted)).await;
    assert!(voice.is_speaking());
    let player = read_pid(&fx.path("tts-pid")).await;

    // The user's speech (or our own echo) arrives while we talk: dropped.
    std::fs::write(fx.path("go1"), "").unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(drain(&mut events), Vec::new());

    voice.stop_speaking();
    wait_for(&mut events, is(LocalVoiceEvent::SpeakingFinished)).await;
    wait_pid_gone(player).await;

    // After the echo tail, speech is heard again.
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(fx.path("go2"), "").unwrap();
    let seen = wait_for(&mut events, is_transcript).await;
    assert_eq!(
        seen,
        vec![
            LocalVoiceEvent::SpeechStarted,
            LocalVoiceEvent::SpeechEnded,
            LocalVoiceEvent::Transcript {
                text: "second burst".to_string()
            },
        ]
    );
    voice.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn barge_in_stops_playback_and_starts_an_utterance() {
    let fx = Fixture::new();
    let mut loud = tone(1000, 9000.0);
    loud.extend(crate::test_support::silence(1200, 5));
    let raw = fx.write_raw("loud.raw", &loud);
    let mut config = config(
        fx.staged_capture(&[("go", &raw)]),
        fx.stt("stop that"),
        fx.speaker(30),
    );
    config.playback.barge_in = true;
    let (voice, mut events) = LocalVoice::start(config);
    wait_for(&mut events, is(LocalVoiceEvent::Listening)).await;
    voice.speak("A long answer.");
    wait_for(&mut events, is(LocalVoiceEvent::SpeakingStarted)).await;
    let player = read_pid(&fx.path("tts-pid")).await;
    std::fs::write(fx.path("go"), "").unwrap();
    let seen = wait_for(&mut events, is_transcript).await;
    assert!(
        seen.contains(&LocalVoiceEvent::SpeakingFinished),
        "{seen:?}"
    );
    assert!(seen.contains(&LocalVoiceEvent::SpeechStarted), "{seen:?}");
    wait_pid_gone(player).await;
    voice.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speak_queues_in_order_and_stop_drops_the_queue() {
    let fx = Fixture::new();
    let (voice, mut events) = LocalVoice::start(config(no_capture(), fx.stt("x"), fx.speaker(0)));
    voice.speak("one");
    voice.speak("two");
    voice.speak("three");
    for _ in 0..3 {
        wait_for(&mut events, is(LocalVoiceEvent::SpeakingFinished)).await;
    }
    assert_eq!(
        std::fs::read_to_string(fx.path("tts-log")).unwrap(),
        "one\ntwo\nthree\n"
    );
    voice.shutdown().await;

    let fx = Fixture::new();
    let (voice, mut events) = LocalVoice::start(config(no_capture(), fx.stt("x"), fx.speaker(30)));
    voice.speak("first");
    voice.speak("second");
    wait_for(&mut events, is(LocalVoiceEvent::SpeakingStarted)).await;
    let player = read_pid(&fx.path("tts-pid")).await;
    voice.stop_speaking();
    wait_for(&mut events, is(LocalVoiceEvent::SpeakingFinished)).await;
    wait_pid_gone(player).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(drain(&mut events), Vec::new());
    assert_eq!(
        std::fs::read_to_string(fx.path("tts-log")).unwrap(),
        "first\n"
    );

    // Speech queued after a stop plays normally.
    std::fs::remove_file(fx.path("tts-pid")).unwrap();
    voice.speak("third");
    wait_for(&mut events, is(LocalVoiceEvent::SpeakingStarted)).await;
    let player = read_pid(&fx.path("tts-pid")).await;
    voice.shutdown().await;
    wait_pid_gone(player).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tts_text_reaches_commands_as_one_argument() {
    let tricky = "He said \"hi\"; echo pwned $(touch /tmp/pwned) `id` & | > 'q' {wav}";
    let fx = Fixture::new();
    let (voice, mut events) = LocalVoice::start(config(no_capture(), fx.stt("x"), fx.speaker(0)));
    voice.speak(format!("-- {tricky}"));
    wait_for(&mut events, is(LocalVoiceEvent::SpeakingFinished)).await;
    // Leading dashes are dropped so the text is never parsed as an option.
    assert_eq!(read_argv(&fx.path("tts-argv")), vec![tricky.to_string()]);
    voice.shutdown().await;

    // command_to_file + player.
    let fx = Fixture::new();
    let say = fx.script(
        "fake-say-to-file",
        &format!(
            "printf '%s\\0' \"$@\" > '{}'\nprintf 'RIFFfake' > \"$2\"",
            fx.path("say-argv").display()
        ),
    );
    let play = fx.script(
        "fake-afplay",
        &format!(
            "printf '%s\\0' \"$@\" > '{}'\ncp \"$1\" '{}'",
            fx.path("play-argv").display(),
            fx.path("played.wav").display()
        ),
    );
    let mut config = config(
        no_capture(),
        fx.stt("x"),
        TtsConfig::CommandToFile(CommandToFileTtsConfig {
            command: vec![
                say,
                "-o".to_string(),
                "{wav}".to_string(),
                "--data-format=LEI16@22050".to_string(),
                "{text}".to_string(),
            ],
            timeout_ms: 10_000,
        }),
    );
    config.playback = PlaybackConfig {
        player: vec![play, "{wav}".to_string()],
        ..PlaybackConfig::default()
    };
    let (voice, mut events) = LocalVoice::start(config);
    voice.speak(tricky);
    let seen = wait_for(&mut events, is(LocalVoiceEvent::SpeakingFinished)).await;
    assert_eq!(seen[0], LocalVoiceEvent::SpeakingStarted);
    let say_argv = read_argv(&fx.path("say-argv"));
    assert_eq!(say_argv.len(), 4);
    assert_eq!(say_argv[0], "-o");
    assert_eq!(say_argv[2], "--data-format=LEI16@22050");
    assert_eq!(say_argv[3], tricky);
    let wav = say_argv[1].clone();
    assert!(wav.ends_with(".wav"), "{wav}");
    assert_eq!(read_argv(&fx.path("play-argv")), vec![wav.clone()]);
    assert_eq!(std::fs::read(fx.path("played.wav")).unwrap(), b"RIFFfake");
    // The synthesized file is removed after playback.
    assert!(!Path::new(&wav).exists());
    assert!(!Path::new("/tmp/pwned").exists());
    voice.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_text_is_capped() {
    let fx = Fixture::new();
    let mut config = config(no_capture(), fx.stt("x"), fx.speaker(0));
    config.playback.max_chars = 40;
    let (voice, mut events) = LocalVoice::start(config);
    voice.speak("First sentence is short. The second sentence goes on and on and on.");
    wait_for(&mut events, is(LocalVoiceEvent::SpeakingFinished)).await;
    assert_eq!(
        read_argv(&fx.path("tts-argv")),
        vec!["First sentence is short.".to_string()]
    );
    voice.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_kills_capture_and_player() {
    let fx = Fixture::new();
    let raw = fx.write_raw("burst.raw", &speech_burst());
    let (voice, mut events) = LocalVoice::start(config(
        fx.staged_capture(&[("never", &raw)]),
        fx.stt("x"),
        fx.speaker(30),
    ));
    wait_for(&mut events, is(LocalVoiceEvent::Listening)).await;
    let capture = voice.capture_pid().unwrap();
    voice.speak("this takes a while");
    wait_for(&mut events, is(LocalVoiceEvent::SpeakingStarted)).await;
    let player = read_pid(&fx.path("tts-pid")).await;
    assert!(!pid_gone(capture));
    assert!(!pid_gone(player));

    voice.shutdown().await;
    assert!(pid_gone(capture), "capture {capture} still running");
    assert!(pid_gone(player), "player {player} still running");
    assert_eq!(voice.capture_pid(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn muted_audio_is_ignored() {
    let fx = Fixture::new();
    let raw = fx.write_raw("burst.raw", &speech_burst());
    let (voice, mut events) = LocalVoice::start(config(
        fx.staged_capture(&[("go1", &raw), ("go2", &raw)]),
        fx.stt("unmuted"),
        fx.speaker(0),
    ));
    wait_for(&mut events, is(LocalVoiceEvent::Listening)).await;
    voice.set_muted(true);
    std::fs::write(fx.path("go1"), "").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(drain(&mut events), Vec::new());
    voice.set_muted(false);
    wait_for(&mut events, is(LocalVoiceEvent::Listening)).await;
    std::fs::write(fx.path("go2"), "").unwrap();
    let seen = wait_for(&mut events, is_transcript).await;
    assert_eq!(
        seen.last(),
        Some(&LocalVoiceEvent::Transcript {
            text: "unmuted".to_string()
        })
    );
    voice.shutdown().await;
}
