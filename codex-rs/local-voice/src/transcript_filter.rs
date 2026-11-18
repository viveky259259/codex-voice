//! Drops empty and pure-noise transcripts.

use crate::config::TranscriptFilterConfig;

/// Returns the cleaned transcript, or `None` when it should be dropped.
pub fn clean_transcript(
    text: &str,
    voiced_ms: u32,
    config: &TranscriptFilterConfig,
) -> Option<String> {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return None;
    }
    let key = match_key(&cleaned);
    if key.is_empty() {
        return None;
    }
    if config.drop.iter().any(|item| match_key(item) == key) {
        return None;
    }
    if voiced_ms < config.short_utterance_ms
        && config
            .drop_if_short
            .iter()
            .any(|item| match_key(item) == key)
    {
        return None;
    }
    if config.drop_bracketed && is_only_annotations(&cleaned) {
        return None;
    }
    if is_repetition_loop(&cleaned, config) {
        return None;
    }
    Some(cleaned)
}

/// True for a long transcript made of a few words looped over and over.
fn is_repetition_loop(text: &str, config: &TranscriptFilterConfig) -> bool {
    if config.max_distinct_word_ratio <= 0.0 {
        return false;
    }
    let words: Vec<String> = text
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    if words.len() < config.repeat_min_words.max(2) {
        return false;
    }
    let distinct = words.iter().collect::<std::collections::HashSet<_>>().len();
    (distinct as f32) / (words.len() as f32) <= config.max_distinct_word_ratio
}

fn match_key(text: &str) -> String {
    text.trim()
        .trim_matches(|c: char| c.is_ascii_punctuation() && !matches!(c, '[' | ']' | '(' | ')'))
        .trim()
        .to_lowercase()
}

/// True for text made only of `[...]`/`(...)` groups and punctuation, such
/// as "[BLANK_AUDIO]" or "(wind blowing) [music]".
fn is_only_annotations(text: &str) -> bool {
    let mut depth = 0i32;
    let mut saw_group = false;
    for c in text.chars() {
        match c {
            '[' | '(' => {
                depth += 1;
                saw_group = true;
            }
            ']' | ')' => depth -= 1,
            _ if depth > 0 => {}
            _ if c.is_whitespace() || c.is_ascii_punctuation() => {}
            _ => return false,
        }
    }
    saw_group
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn drops_empty_and_noise_transcripts() {
        let config = TranscriptFilterConfig::default();
        assert_eq!(clean_transcript("  \n ", 2000, &config), None);
        assert_eq!(clean_transcript("[BLANK_AUDIO]", 2000, &config), None);
        assert_eq!(clean_transcript(" (silence) ", 2000, &config), None);
        assert_eq!(
            clean_transcript("(wind blowing) [Music]", 2000, &config),
            None
        );
        assert_eq!(clean_transcript("...", 2000, &config), None);
        assert_eq!(clean_transcript("Thank you.", 400, &config), None);
        assert_eq!(
            clean_transcript(
                "Það er það er það er það. Það er það er það. Það er það er það.",
                9000,
                &config
            ),
            None
        );
        // Ordinary sentences with some repeated words are kept.
        assert_eq!(
            clean_transcript(
                "run the tests and then run the linter on the tests",
                3000,
                &config
            ),
            Some("run the tests and then run the linter on the tests".to_string())
        );
        assert_eq!(clean_transcript("thank you", 400, &config), None);
        assert_eq!(
            clean_transcript("Thank you.", 1500, &config),
            Some("Thank you.".to_string())
        );
        assert_eq!(
            clean_transcript(" run the\n tests [laughs] ", 900, &config),
            Some("run the tests [laughs]".to_string())
        );
        assert_eq!(
            clean_transcript("yes", 300, &config),
            Some("yes".to_string())
        );
    }

    #[test]
    fn drop_list_is_configurable() {
        let config = TranscriptFilterConfig {
            drop: vec!["um".to_string()],
            drop_if_short: Vec::new(),
            short_utterance_ms: 600,
            drop_bracketed: false,
            max_distinct_word_ratio: 0.0,
            repeat_min_words: 8,
        };
        assert_eq!(clean_transcript("Um.", 2000, &config), None);
        assert_eq!(
            clean_transcript("Thank you.", 100, &config),
            Some("Thank you.".to_string())
        );
        assert_eq!(
            clean_transcript("[BLANK_AUDIO]", 2000, &config),
            Some("[BLANK_AUDIO]".to_string())
        );
    }
}
