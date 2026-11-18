use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::silence;
use crate::test_support::tone;
use crate::test_support::voice;

fn run(vad: &mut Vad, samples: &[i16]) -> Vec<VadEvent> {
    // Feed in uneven chunks to exercise frame reassembly.
    let mut events = Vec::new();
    for chunk in samples.chunks(1234) {
        events.extend(vad.push(chunk));
    }
    events
}

fn utterances(events: &[VadEvent]) -> Vec<&Utterance> {
    events
        .iter()
        .filter_map(|event| match event {
            VadEvent::SpeechEnded(utterance) => Some(utterance),
            VadEvent::SpeechStarted | VadEvent::SpeechDiscarded => None,
        })
        .collect()
}

fn starts(events: &[VadEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, VadEvent::SpeechStarted))
        .count()
}

#[test]
fn silence_produces_no_events() {
    let mut vad = Vad::new(VadConfig::default());
    let events = run(&mut vad, &silence(3000, 1));
    assert_eq!(events, Vec::new());
    assert!(vad.flush().is_empty());
}

#[test]
fn tone_burst_is_one_utterance_with_pre_roll() {
    let mut input = silence(1000, 2);
    let tone_start = input.len();
    input.extend(voice(1000, 8000.0));
    input.extend(silence(1500, 3));

    let mut vad = Vad::new(VadConfig::default());
    let events = run(&mut vad, &input);
    assert_eq!(starts(&events), 1);
    let found = utterances(&events);
    assert_eq!(found.len(), 1);
    let utterance = found[0];
    assert!(!utterance.truncated);

    // 300 ms of pre-roll, taken from the audio just before the tone.
    let pre_roll = ms_to_samples(300);
    assert_eq!(
        &utterance.samples[..pre_roll],
        &input[tone_start - pre_roll..tone_start]
    );
    assert_eq!(
        &utterance.samples[pre_roll..pre_roll + ms_to_samples(1000)],
        &input[tone_start..tone_start + ms_to_samples(1000)]
    );
    // Pre-roll + tone + the 700 ms of silence that ended it.
    assert_eq!(utterance.duration_ms(), 300 + 1000 + 700);
    assert_eq!(utterance.voiced_ms, 1000);
}

#[test]
fn short_gap_keeps_one_utterance_long_gap_splits() {
    let mut short_gap = silence(600, 4);
    short_gap.extend(tone(500, 6000.0));
    short_gap.extend(silence(300, 5));
    short_gap.extend(tone(500, 6000.0));
    short_gap.extend(silence(1200, 6));
    let mut vad = Vad::new(VadConfig::default());
    let events = run(&mut vad, &short_gap);
    assert_eq!(starts(&events), 1);
    let found = utterances(&events);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].voiced_ms, 1300);

    let mut long_gap = silence(600, 7);
    long_gap.extend(tone(500, 6000.0));
    long_gap.extend(silence(1200, 8));
    long_gap.extend(tone(500, 6000.0));
    long_gap.extend(silence(1200, 9));
    let mut vad = Vad::new(VadConfig::default());
    let events = run(&mut vad, &long_gap);
    assert_eq!(starts(&events), 2);
    let found = utterances(&events);
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].voiced_ms, 500);
    assert_eq!(found[1].voiced_ms, 500);
}

#[test]
fn max_length_cuts_the_utterance() {
    let config = VadConfig {
        max_utterance_ms: 2000,
        ..VadConfig::default()
    };
    let mut input = silence(600, 10);
    input.extend(voice(4500, 7000.0));
    input.extend(silence(1000, 11));
    let mut vad = Vad::new(config);
    let events = run(&mut vad, &input);
    let found = utterances(&events);
    assert!(found.len() >= 2, "got {} utterances", found.len());
    assert!(found[0].truncated);
    assert_eq!(found[0].duration_ms(), 2000);
    assert!(starts(&events) >= 2);
    // A steady, unmodulated tone this long counts as background: the floor
    // rises to it, so no further utterance starts on it before it ends.
    assert!(!vad.in_speech());
}

#[test]
fn clicks_shorter_than_min_speech_are_ignored() {
    let mut input = silence(600, 12);
    input.extend(tone(60, 9000.0));
    input.extend(silence(1500, 13));
    let mut vad = Vad::new(VadConfig::default());
    assert_eq!(run(&mut vad, &input), Vec::new());
}

#[test]
fn noise_floor_adapts_to_steady_background() {
    // Loud steady background from the start: calibration takes it as floor.
    let mut input = tone(2000, 400.0);
    let mut vad = Vad::new(VadConfig::default());
    assert_eq!(run(&mut vad, &input), Vec::new());
    assert!(vad.noise_floor() > 200.0, "floor {}", vad.noise_floor());

    input = voice(800, 6000.0);
    input.extend(tone(1500, 400.0));
    let events = run(&mut vad, &input);
    assert_eq!(starts(&events), 1);
    assert_eq!(utterances(&events).len(), 1);
}

#[test]
fn flush_returns_speech_in_progress() {
    let mut input = silence(600, 14);
    input.extend(voice(800, 8000.0));
    let mut vad = Vad::new(VadConfig::default());
    let events = run(&mut vad, &input);
    assert_eq!(starts(&events), 1);
    let flushed = vad.flush();
    assert_eq!(utterances(&flushed).len(), 1);
}

#[test]
fn barge_in_needs_sustained_loud_audio() {
    let mut detector = BargeInDetector::new(20.0, 300, ms_to_samples(20));
    // 200 ms loud is not enough.
    assert_eq!(detector.push(&tone(200, 9000.0), 60.0), None);
    assert_eq!(detector.push(&silence(100, 15), 60.0), None);
    let audio = detector.push(&tone(400, 9000.0), 60.0).unwrap();
    assert!(samples_to_ms(audio.len()) >= 300);
}

/// Steady broadband noise: uniform random samples in ±amplitude.
fn noise(ms: u32, amplitude: i16, seed: u32) -> Vec<i16> {
    let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    let span = (amplitude as u32) * 2 + 1;
    (0..ms_to_samples(ms))
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            ((state >> 8) % span) as i16 - amplitude
        })
        .collect()
}

#[test]
fn a_step_up_in_steady_noise_is_discarded_instead_of_running_to_max() {
    // Calibrates on near-digital silence (a virtual mic warming up), then the
    // room's steady noise arrives well above that floor and never stops.
    let mut input = silence(500, 20);
    input.extend(noise(12_000, 1500, 21));
    let mut vad = Vad::new(VadConfig::default());
    let events = run(&mut vad, &input);

    // The steady noise is recognised and discarded within a second or so:
    // nothing to transcribe, no 30 s wait.
    assert_eq!(utterances(&events).len(), 0, "{events:?}");
    assert!(events.contains(&VadEvent::SpeechDiscarded), "{events:?}");
    // The floor now sits at the noise, so it doesn't restart on it.
    assert_eq!(starts(&events), 1);
    assert!(!vad.in_speech());

    // Speech over the noise is still detected and ends normally.
    let mut speech = voice(1000, 12_000.0);
    speech.extend(noise(1500, 1500, 22));
    let events = run(&mut vad, &speech);
    assert_eq!(starts(&events), 1);
    assert_eq!(utterances(&events).len(), 1);
    assert!(!utterances(&events)[0].truncated);
}

#[test]
fn long_speech_with_pauses_between_words_is_not_cut_by_the_floor() {
    // 8 s of "words" (250 ms loud, 150 ms quiet): pauses are shorter than
    // end_silence_ms, and frequent enough to keep the floor at background.
    let mut input = silence(600, 23);
    for index in 0..20 {
        input.extend(tone(250, 7000.0));
        input.extend(silence(150, 30 + index));
    }
    input.extend(silence(1200, 60));
    let mut vad = Vad::new(VadConfig::default());
    let events = run(&mut vad, &input);
    assert_eq!(starts(&events), 1);
    let found = utterances(&events);
    assert_eq!(found.len(), 1);
    assert!(
        found[0].voiced_ms >= 7_500,
        "voiced {} ms",
        found[0].voiced_ms
    );
}

#[test]
fn quiet_speech_right_after_loud_noise_stops_is_still_heard() {
    // 6 s of loud steady noise (the floor rises to it), then the noise stops
    // and a quiet speaker talks within a second.
    let mut input = silence(500, 70);
    input.extend(noise(6_000, 3000, 71));
    input.extend(silence(800, 72));
    let mut vad = Vad::new(VadConfig::default());
    run(&mut vad, &input);

    let mut speech = voice(800, 1500.0);
    speech.extend(silence(1200, 73));
    let events = run(&mut vad, &speech);
    assert_eq!(starts(&events), 1, "floor {}", vad.noise_floor());
    assert_eq!(utterances(&events).len(), 1);
}

#[test]
fn hiss_after_digital_silence_never_starts_speech() {
    // The Mac's virtual mic: 250 ms of exact zeros, then steady hiss about as
    // loud as speech (around -28 dBFS), and nobody talking.
    let mut input = vec![0i16; ms_to_samples(250)];
    input.extend(noise(16_000, 1400, 80));
    let mut vad = Vad::new(VadConfig::default());
    let events = run(&mut vad, &input);
    assert_eq!(utterances(&events).len(), 0, "{events:?}");
    assert!(!vad.in_speech());
}

#[test]
fn steady_hiss_mistaken_for_speech_is_discarded_not_transcribed() {
    // Calibrated on a genuinely quiet start, then a steady hiss arrives: it
    // starts as "speech", is recognised as steady, and is discarded.
    let mut input = silence(500, 81);
    input.extend(noise(6_000, 1400, 82));
    let mut vad = Vad::new(VadConfig::default());
    let events = run(&mut vad, &input);
    assert_eq!(utterances(&events).len(), 0, "{events:?}");
    assert!(events.contains(&VadEvent::SpeechDiscarded), "{events:?}");
    assert!(starts(&events) <= 1);

    // Real speech over that hiss is still heard.
    let mut speech = voice(1200, 14_000.0);
    speech.extend(noise(1500, 1400, 83));
    let events = run(&mut vad, &speech);
    assert_eq!(utterances(&events).len(), 1, "{events:?}");
}

/// Hiss whose level wanders ±2 dB every 60 ms, like a real headset mic:
/// too jittery for a per-frame steadiness check.
fn jittery_hiss(ms: u32, amplitude: i16, seed: u32) -> Vec<i16> {
    let mut out = Vec::new();
    let mut state = seed.wrapping_mul(2_246_822_519).wrapping_add(7);
    for _ in 0..(ms / 60) {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let gain = 0.8 + 0.4 * ((state >> 16) % 1000) as f32 / 1000.0;
        out.extend(noise(60, (f32::from(amplitude) * gain) as i16, state));
    }
    out
}

#[test]
fn utterance_ends_soon_after_speech_returns_to_the_hiss_before_it() {
    // A genuinely quiet start, then jittery hiss well above that floor for a
    // while (discarded or absorbed), then a sentence, then the same hiss.
    let mut vad = Vad::new(VadConfig::default());
    let mut input = silence(500, 90);
    input.extend(jittery_hiss(5_000, 1400, 91));
    let warmup = run(&mut vad, &input);
    assert_eq!(utterances(&warmup).len(), 0, "{warmup:?}");

    let mut speech = voice(1500, 14_000.0);
    speech.extend(jittery_hiss(4_000, 1400, 92));
    let mut ended_after_ms = None;
    let mut started = 0;
    let mut elapsed = 0u32;
    for chunk in speech.chunks(ms_to_samples(20)) {
        for event in vad.push(chunk) {
            match event {
                VadEvent::SpeechStarted => started += 1,
                VadEvent::SpeechEnded(_) if ended_after_ms.is_none() => {
                    ended_after_ms = Some(elapsed.saturating_sub(1500));
                }
                _ => {}
            }
        }
        elapsed += 20;
    }
    let ended_after_ms = ended_after_ms.expect("utterance never ended");
    assert!(
        ended_after_ms <= 1_200,
        "ended {ended_after_ms} ms after the speech stopped"
    );
    // The hiss after it doesn't start another utterance.
    assert_eq!(started, 1);
}
