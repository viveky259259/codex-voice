use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::silence;
use crate::test_support::tone;

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
            VadEvent::SpeechStarted => None,
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
    input.extend(tone(1000, 8000.0));
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
    input.extend(tone(4500, 7000.0));
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

    input = tone(800, 6000.0);
    input.extend(tone(1500, 400.0));
    let events = run(&mut vad, &input);
    assert_eq!(starts(&events), 1);
    assert_eq!(utterances(&events).len(), 1);
}

#[test]
fn flush_returns_speech_in_progress() {
    let mut input = silence(600, 14);
    input.extend(tone(800, 8000.0));
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
fn a_step_up_in_steady_noise_ends_the_utterance_instead_of_running_to_max() {
    // Calibrates on near-digital silence (a virtual mic warming up), then the
    // room's steady noise arrives well above that floor and never stops.
    let mut input = silence(500, 20);
    input.extend(noise(12_000, 1500, 21));
    let mut vad = Vad::new(VadConfig::default());
    let events = run(&mut vad, &input);

    let found = utterances(&events);
    assert_eq!(found.len(), 1, "{} utterances", found.len());
    assert!(!found[0].truncated, "ran to max_utterance_ms");
    assert!(
        found[0].duration_ms() <= 6_000,
        "ended after {} ms",
        found[0].duration_ms()
    );
    // The floor now sits at the noise, so it doesn't restart on it.
    assert_eq!(starts(&events), 1);
    assert!(!vad.in_speech());

    // Speech over the noise is still detected and ends normally.
    let mut speech = tone(1000, 12_000.0);
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

    let mut speech = tone(800, 1500.0);
    speech.extend(silence(1200, 73));
    let events = run(&mut vad, &speech);
    assert_eq!(starts(&events), 1, "floor {}", vad.noise_floor());
    assert_eq!(utterances(&events).len(), 1);
}
