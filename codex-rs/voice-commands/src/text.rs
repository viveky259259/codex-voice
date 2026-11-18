//! Word splitting and normalization shared by the grammar and name matching.

/// One spoken word, lowercased, with its byte offset in the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Word {
    pub(crate) text: String,
    pub(crate) start: usize,
}

const NUMBER_WORDS: &[(&str, &str)] = &[
    ("zero", "0"),
    ("one", "1"),
    ("two", "2"),
    ("three", "3"),
    ("four", "4"),
    ("five", "5"),
    ("six", "6"),
    ("seven", "7"),
    ("eight", "8"),
    ("nine", "9"),
    ("ten", "10"),
    ("eleven", "11"),
    ("twelve", "12"),
    ("thirteen", "13"),
    ("fourteen", "14"),
    ("fifteen", "15"),
    ("sixteen", "16"),
    ("seventeen", "17"),
    ("eighteen", "18"),
    ("nineteen", "19"),
    ("twenty", "20"),
];

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '\'' | '\u{2019}' | '\u{2018}')
}

/// Split a transcript into lowercase words. Punctuation, hyphens, and
/// underscores separate words; "what's" and "how's" expand to two words.
pub(crate) fn spoken_words(transcript: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut start = None;
    let ends = transcript
        .char_indices()
        .chain(std::iter::once((transcript.len(), ' ')));
    for (index, ch) in ends {
        if is_word_char(ch) {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            push_word(&mut words, &transcript[begin..index], begin);
        }
    }
    words
}

fn push_word(words: &mut Vec<Word>, raw: &str, start: usize) {
    let lowered = raw.to_lowercase().replace(['\u{2019}', '\u{2018}'], "'");
    let text = lowered.trim_matches('\'');
    let expanded: &[&str] = match text {
        "" => &[],
        "what's" | "whats" => &["what", "is"],
        "how's" => &["how", "is"],
        "who's" => &["who", "is"],
        other => {
            words.push(Word {
                text: other.to_string(),
                start,
            });
            return;
        }
    };
    words.extend(expanded.iter().map(|text| Word {
        text: (*text).to_string(),
        start,
    }));
}

/// Split one name or word into lowercase pieces at separators, camelCase
/// humps, and letter/digit boundaries: "MacStudio-2" -> mac, studio, 2.
fn pieces(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut previous: Option<char> = None;
    for ch in text.chars() {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                pieces.push(std::mem::take(&mut current));
            }
            previous = None;
            continue;
        }
        let boundary = previous.is_some_and(|prev| {
            (prev.is_lowercase() && ch.is_uppercase())
                || (prev.is_alphabetic() && ch.is_numeric())
                || (prev.is_numeric() && ch.is_alphabetic())
        });
        if boundary && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
        }
        current.extend(ch.to_lowercase());
        previous = Some(ch);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

/// Normalize words for matching: split into pieces, read number words as
/// digits, and drop "point"/"dot" between digits ("five point five" -> 5 5).
pub(crate) fn normalize<'a>(words: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut normalized: Vec<String> = words
        .into_iter()
        .flat_map(pieces)
        .map(|piece| {
            NUMBER_WORDS
                .iter()
                .find(|(word, _)| *word == piece)
                .map_or(piece, |(_, digit)| (*digit).to_string())
        })
        .collect();
    let is_digit =
        |word: Option<&String>| word.is_some_and(|word| word.chars().all(|ch| ch.is_ascii_digit()));
    let mut index = 1;
    while index + 1 < normalized.len() {
        if matches!(normalized[index].as_str(), "point" | "dot")
            && is_digit(normalized.get(index - 1))
            && is_digit(normalized.get(index + 1))
        {
            normalized.remove(index);
        } else {
            index += 1;
        }
    }
    normalized
}

/// Normalized words of a display name such as "mac-studio" or "gpt-5.5".
pub(crate) fn name_words(name: &str) -> Vec<String> {
    normalize(name.split_whitespace())
}

/// A separator-free key: "mac studio", "MacStudio", and "mac_studio" share one.
pub(crate) fn key(words: &[String]) -> String {
    words.concat()
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
