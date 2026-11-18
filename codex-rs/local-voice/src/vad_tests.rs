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
    assert!(!found.last().unwrap().truncated);
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
