//! Helpers shared by the unit tests: synthetic PCM and fake commands.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::LocalVoiceEvent;
use crate::vad::ms_to_samples;

/// Low-level pseudo-random noise (about 12 RMS on the i16 scale).
pub(crate) fn silence(ms: u32, seed: u32) -> Vec<i16> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..ms_to_samples(ms))
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            ((state >> 16) % 41) as i16 - 20
        })
        .collect()
}

/// A 440 Hz sine with the given peak amplitude.
pub(crate) fn tone(ms: u32, amplitude: f32) -> Vec<i16> {
    (0..ms_to_samples(ms))
        .map(|index| {
            let t = index as f32 / 16_000.0;
            (amplitude * (2.0 * std::f32::consts::PI * 440.0 * t).sin()) as i16
        })
        .collect()
}

pub(crate) fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
}

/// Leading silence, a one-second tone, then enough silence to end it.
pub(crate) fn speech_burst() -> Vec<i16> {
    let mut samples = silence(600, 21);
    samples.extend(tone(1000, 8000.0));
    samples.extend(silence(1200, 22));
    samples
}

/// Writes an executable `/bin/sh` script.
pub(crate) fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Reads the NUL-separated argv a fake script recorded with
/// `printf '%s\0' "$@" > file`.
pub(crate) fn read_argv(path: &Path) -> Vec<String> {
    let bytes = std::fs::read(path).unwrap();
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

pub(crate) fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Waits for an event matching `pred`, returning the events skipped on the
/// way (including the match, last).
pub(crate) async fn wait_for(
    events: &mut mpsc::Receiver<LocalVoiceEvent>,
    pred: impl Fn(&LocalVoiceEvent) -> bool,
) -> Vec<LocalVoiceEvent> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(event)) => {
                let done = pred(&event);
                seen.push(event);
                if done {
                    return seen;
                }
            }
            Ok(None) => panic!("event stream closed; saw {seen:?}"),
            Err(_) => panic!("timed out waiting for event; saw {seen:?}"),
        }
    }
}

pub(crate) async fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// True once the process is gone (or only a zombie awaiting its reaper).
///
/// Uses `ps` rather than `/proc`, which does not exist on macOS.
pub(crate) fn pid_gone(pid: u32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    let stat = String::from_utf8_lossy(&output.stdout);
    let stat = stat.trim();
    stat.is_empty() || stat.starts_with('Z')
}

pub(crate) async fn wait_pid_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pid_gone(pid) {
        assert!(Instant::now() < deadline, "process {pid} still running");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
