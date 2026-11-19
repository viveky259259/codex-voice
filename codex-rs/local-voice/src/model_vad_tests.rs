use std::io::Write;

use pretty_assertions::assert_eq;

use super::*;

const FRAME: usize = 512; // 32 ms

fn config() -> (VadConfig, ModelVadConfig) {
    let vad = VadConfig {
        min_speech_ms: 64,   // 2 frames
        end_silence_ms: 160, // 5 frames
        max_utterance_ms: 30_000,
        pre_roll_ms: 64, // 2 frames
        ..VadConfig::default()
    };
    (vad, ModelVadConfig::default())
}

fn frame(value: i16) -> Vec<i16> {
    vec![value; FRAME]
}

/// Feeds one frame per probability; frame `i` holds the sample value `i`.
fn run(vad: &mut ProbabilityVad, probabilities: &[f32]) -> Vec<(usize, VadEvent)> {
    let mut out = Vec::new();
    for (i, p) in probabilities.iter().enumerate() {
        for event in vad.push_frame(&frame(i as i16), *p) {
            out.push((i, event));
        }
    }
    out
}

fn first_samples_of_each_frame(utterance: &Utterance) -> Vec<i16> {
    utterance.samples.chunks(FRAME).map(|f| f[0]).collect()
}

#[test]
fn speech_starts_after_min_speech_and_ends_after_end_silence() {
    let (vad_config, model) = config();
    let mut vad = ProbabilityVad::new(&vad_config, &model);
    let probabilities = [
        0.0, 0.0, 0.0, // background (frames 1 and 2 become the pre-roll)
        0.9, 0.9, 0.8, 0.9, // speech
        0.1, 0.1, 0.1, 0.1, 0.1, // 5 unvoiced frames end it
        0.0,
    ];
    let events = run(&mut vad, &probabilities);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], (4, VadEvent::SpeechStarted));
    let (at, VadEvent::SpeechEnded(utterance)) = &events[1] else {
        panic!("expected SpeechEnded, got {events:?}");
    };
    assert_eq!(*at, 11);
    // Pre-roll (1, 2), speech (3..=6), trailing silence (7..=11).
    assert_eq!(
        first_samples_of_each_frame(utterance),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
    );
    assert_eq!(utterance.voiced_ms, 128); // 4 voiced frames of 32 ms
    assert!(!utterance.truncated);
    assert!(!vad.in_speech());
}

#[test]
fn a_blip_shorter_than_min_speech_is_not_speech() {
    let (vad_config, model) = config();
    let mut vad = ProbabilityVad::new(&vad_config, &model);
    let events = run(&mut vad, &[0.0, 0.9, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(events, Vec::new());
}

#[test]
fn pauses_shorter_than_end_silence_keep_one_utterance() {
    let (vad_config, model) = config();
    let mut vad = ProbabilityVad::new(&vad_config, &model);
    let mut probabilities = vec![0.9; 6];
    probabilities.extend([0.2; 4]); // a pause between words, under 5 frames
    probabilities.extend([0.9; 6]);
    probabilities.extend([0.0; 5]);
    let events = run(&mut vad, &probabilities);
    let ended: Vec<_> = events
        .iter()
        .filter(|(_, e)| matches!(e, VadEvent::SpeechEnded(_)))
        .collect();
    assert_eq!(ended.len(), 1, "{events:?}");
    let VadEvent::SpeechEnded(utterance) = &ended[0].1 else {
        unreachable!()
    };
    assert_eq!(utterance.voiced_ms, 16 * 32);
}

#[test]
fn probabilities_between_the_thresholds_continue_but_do_not_start() {
    let (vad_config, model) = config();
    let mut vad = ProbabilityVad::new(&vad_config, &model);
    // 0.4 is under start (0.5) but over continue (0.35).
    assert_eq!(run(&mut vad, &[0.4; 10]), Vec::new());
    let mut probabilities = vec![0.9, 0.9];
    probabilities.extend([0.4; 10]);
    let events = run(&mut vad, &probabilities);
    assert_eq!(events, vec![(1, VadEvent::SpeechStarted)]);
    assert!(vad.in_speech());
}

#[test]
fn long_speech_is_cut_at_max_utterance() {
    let (mut vad_config, model) = config();
    vad_config.max_utterance_ms = 320; // 10 frames
    vad_config.pre_roll_ms = 0;
    let mut vad = ProbabilityVad::new(&vad_config, &model);
    let events = run(&mut vad, &[0.9; 12]);
    let (at, VadEvent::SpeechEnded(utterance)) = &events[1] else {
        panic!("{events:?}");
    };
    assert_eq!(*at, 9);
    assert!(utterance.truncated);
    assert_eq!(utterance.samples.len(), 10 * FRAME);
}

#[test]
fn flush_returns_the_utterance_in_progress() {
    let (vad_config, model) = config();
    let mut vad = ProbabilityVad::new(&vad_config, &model);
    run(&mut vad, &[0.9, 0.9, 0.9]);
    let events = vad.flush();
    assert!(
        matches!(events.as_slice(), [VadEvent::SpeechEnded(u)] if u.samples.len() == 3 * FRAME),
        "{events:?}"
    );
    assert_eq!(vad.flush(), Vec::new());
}

#[test]
fn reset_drops_speech_and_pre_roll() {
    let (vad_config, model) = config();
    let mut vad = ProbabilityVad::new(&vad_config, &model);
    run(&mut vad, &[0.0, 0.0, 0.9, 0.9]);
    vad.reset();
    assert!(!vad.in_speech());
    let events = run(&mut vad, &[0.9, 0.9, 0.0, 0.0, 0.0, 0.0, 0.0]);
    let VadEvent::SpeechEnded(utterance) = &events[1].1 else {
        panic!("{events:?}");
    };
    // No pre-roll survived the reset.
    assert_eq!(utterance.samples.len(), 7 * FRAME);
}

#[test]
fn backend_and_model_parse_from_toml() {
    let config: VadConfig = toml::from_str(
        r#"
backend = "model"
end_silence_ms = 500
[model]
command = ["uv", "run", "--script", "silero_vad.py"]
start_probability = 0.6
"#,
    )
    .unwrap();
    assert_eq!(config.backend, VadBackend::Model);
    assert_eq!(config.end_silence_ms, 500);
    assert_eq!(
        config.model,
        ModelVadConfig {
            command: vec![
                "uv".to_string(),
                "run".to_string(),
                "--script".to_string(),
                "silero_vad.py".to_string()
            ],
            start_probability: 0.6,
            ..ModelVadConfig::default()
        }
    );
    assert_eq!(VadConfig::default().backend, VadBackend::Auto);
}

/// A stand-in helper: `ready`, then 1.000 for frames whose first sample is
/// non-zero and 0.000 otherwise.
fn fake_helper(dir: &std::path::Path, body: &str) -> Vec<String> {
    let path = dir.join("helper.py");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(body.as_bytes()).unwrap();
    vec![
        "python3".to_string(),
        "-I".to_string(),
        path.to_string_lossy().into_owned(),
    ]
}

const ECHO_HELPER: &str = r#"
import sys
out = sys.stdout
out.write("ready\n"); out.flush()
while True:
    data = sys.stdin.buffer.read(1024)
    if len(data) < 1024:
        break
    first = int.from_bytes(data[0:2], "little", signed=True)
    out.write("1.000\n" if first else "0.000\n"); out.flush()
"#;

#[tokio::test]
async fn the_helper_returns_one_probability_per_frame() {
    let dir = tempfile::tempdir().unwrap();
    let mut model = VadModel::spawn(&ModelVadConfig {
        command: fake_helper(dir.path(), ECHO_HELPER),
        ..ModelVadConfig::default()
    })
    .await
    .unwrap();
    assert_eq!(model.probability(&frame(0)).await.unwrap(), 0.0);
    assert_eq!(model.probability(&frame(7)).await.unwrap(), 1.0);
    assert_eq!(model.probability(&frame(0)).await.unwrap(), 0.0);
    model.stop().await;
}

#[tokio::test]
async fn a_missing_command_is_a_config_error() {
    let Err(err) = VadModel::spawn(&ModelVadConfig::default()).await else {
        panic!("expected an error");
    };
    assert!(matches!(err, VoiceError::Config(_)), "{err}");
    assert!(err.to_string().contains("silero_vad.py"), "{err}");
}

#[tokio::test]
async fn a_helper_that_fails_reports_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let command = fake_helper(
        dir.path(),
        "import sys\nsys.stderr.write('no model here\\n')\nsys.exit(3)\n",
    );
    let Err(err) = VadModel::spawn(&ModelVadConfig {
        command,
        ..ModelVadConfig::default()
    })
    .await
    else {
        panic!("expected an error");
    };
    let message = err.to_string();
    assert!(message.contains("exited"), "{message}");
    assert!(message.contains("no model here"), "{message}");
}

#[tokio::test]
async fn a_helper_that_never_gets_ready_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let command = fake_helper(dir.path(), "import time\ntime.sleep(30)\n");
    let Err(err) = VadModel::spawn(&ModelVadConfig {
        command,
        startup_timeout_ms: 200,
        ..ModelVadConfig::default()
    })
    .await
    else {
        panic!("expected an error");
    };
    assert!(matches!(err, VoiceError::Timeout(200)), "{err}");
}

fn host(macos: bool, uv: bool, dir: &std::path::Path) -> Host {
    Host {
        macos,
        uv: uv.then(|| std::path::PathBuf::from("/opt/test/uv")),
        cache_dir: Some(dir.to_path_buf()),
    }
}

fn with_backend(backend: VadBackend) -> VadConfig {
    VadConfig {
        backend,
        ..VadConfig::default()
    }
}

fn bundled(dir: &std::path::Path) -> Vec<String> {
    vec![
        "/opt/test/uv".to_string(),
        "run".to_string(),
        "--quiet".to_string(),
        "--script".to_string(),
        dir.join("silero_vad.py").to_string_lossy().into_owned(),
    ]
}

#[test]
fn auto_uses_the_bundled_model_on_macos_with_uv() {
    let dir = tempfile::tempdir().unwrap();
    let plan = plan(
        &with_backend(VadBackend::Auto),
        &host(true, true, dir.path()),
    )
    .unwrap();
    assert_eq!(
        plan,
        DetectorPlan::Model {
            command: bundled(dir.path()),
            fallback_to_energy: true,
            startup_timeout_ms: 60_000,
        }
    );
    // The helper was written next to where it keeps the model.
    let written = std::fs::read_to_string(dir.path().join("silero_vad.py")).unwrap();
    assert!(written.contains("Silero VAD v5"));
}

#[test]
fn auto_uses_energy_without_uv_or_off_macos() {
    let dir = tempfile::tempdir().unwrap();
    let auto = with_backend(VadBackend::Auto);
    assert_eq!(
        plan(&auto, &host(true, false, dir.path())).unwrap(),
        DetectorPlan::Energy
    );
    assert_eq!(
        plan(&auto, &host(false, true, dir.path())).unwrap(),
        DetectorPlan::Energy
    );
    assert!(!dir.path().join("silero_vad.py").exists());
}

#[test]
fn auto_with_a_configured_command_uses_it_anywhere_with_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = with_backend(VadBackend::Auto);
    config.model.command = vec!["my-vad".to_string()];
    assert_eq!(
        plan(&config, &host(false, false, dir.path())).unwrap(),
        DetectorPlan::Model {
            command: vec!["my-vad".to_string()],
            fallback_to_energy: true,
            startup_timeout_ms: 60_000,
        }
    );
}

#[test]
fn model_is_strict_and_energy_is_energy() {
    let dir = tempfile::tempdir().unwrap();
    let model = with_backend(VadBackend::Model);
    assert_eq!(
        plan(&model, &host(false, true, dir.path())).unwrap(),
        DetectorPlan::Model {
            command: bundled(dir.path()),
            fallback_to_energy: false,
            startup_timeout_ms: 120_000,
        }
    );
    let Err(err) = plan(&model, &host(true, false, dir.path())) else {
        panic!("model without uv or a command must fail");
    };
    assert!(err.to_string().contains("uv"), "{err}");
    assert_eq!(
        plan(
            &with_backend(VadBackend::Energy),
            &host(true, true, dir.path())
        )
        .unwrap(),
        DetectorPlan::Energy
    );
}

#[test]
fn the_bundled_helper_is_rewritten_only_when_it_differs() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_bundled_helper(dir.path()).unwrap();
    std::fs::write(&path, "stale").unwrap();
    write_bundled_helper(dir.path()).unwrap();
    assert_ne!(std::fs::read_to_string(&path).unwrap(), "stale");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    write_bundled_helper(dir.path()).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
}
