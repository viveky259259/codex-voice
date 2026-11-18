use super::*;
use pretty_assertions::assert_eq;

fn texts(transcript: &str) -> Vec<String> {
    spoken_words(transcript)
        .into_iter()
        .map(|word| word.text)
        .collect()
}

#[test]
fn spoken_words_lowercase_split_and_expand() {
    assert_eq!(
        texts("What’s build-frontend doing?"),
        vec!["what", "is", "build", "frontend", "doing"]
    );
    assert_eq!(texts("  'quoted'  ok.  "), vec!["quoted", "ok"]);
    assert_eq!(texts("fix parser.rs"), vec!["fix", "parser", "rs"]);
    assert_eq!(texts(""), Vec::<String>::new());
}

#[test]
fn spoken_words_keep_byte_offsets() {
    let words = spoken_words("tell build to Fix parser.rs");
    let starts: Vec<usize> = words.iter().map(|word| word.start).collect();
    assert_eq!(starts, vec![0, 5, 11, 14, 18, 25]);
}

#[test]
fn name_words_split_camel_case_and_digits() {
    assert_eq!(name_words("MacStudio"), vec!["mac", "studio"]);
    assert_eq!(name_words("gpt-5.5"), vec!["gpt", "5", "5"]);
    assert_eq!(name_words("worker_2"), vec!["worker", "2"]);
    assert_eq!(name_words("gpt5"), vec!["gpt", "5"]);
}

#[test]
fn normalize_reads_number_words_and_points() {
    assert_eq!(
        normalize(["gpt", "five", "point", "five"]),
        vec!["gpt", "5", "5"]
    );
    assert_eq!(normalize(["worker", "two"]), vec!["worker", "2"]);
    // "point" only disappears between digits.
    assert_eq!(normalize(["the", "point"]), vec!["the", "point"]);
}

#[test]
fn keys_ignore_separators() {
    assert_eq!(
        key(&name_words("mac studio")),
        key(&name_words("MacStudio"))
    );
    assert_eq!(key(&name_words("mac_studio")), "macstudio");
    assert_eq!(key(&normalize(["gpt", "five"])), key(&name_words("gpt-5")));
}
