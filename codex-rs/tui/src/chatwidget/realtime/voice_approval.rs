//! Pure grammar and phrasing for answering approval prompts by voice.
//!
//! Matching is deliberately conservative: an utterance must be short and, after
//! dropping a few politeness fillers, equal one of the fixed phrases below.
//! Substrings never match, so "approve the migration later" or "don't approve"
//! are left for the model. Spoken text stays short and never includes raw diffs
//! or long commands.

use crate::bottom_pane::ApprovalRequest;
use crate::bottom_pane::VoiceApprovalOption;
use crate::exec_command::strip_bash_lc_and_escape;
use codex_app_server_protocol::CommandExecutionApprovalKind;

/// Utterances longer than this are conversation, not approval answers.
const MAX_INTENT_WORDS: usize = 6;
/// Spoken status text is capped so it stays a short interjection.
pub(crate) const MAX_SPOKEN_WORDS: usize = 25;
const MAX_SUMMARY_WORDS: usize = 6;
const MAX_SUMMARY_CHARS: usize = 48;
const MAX_ERROR_WORDS: usize = 14;

/// An approval answer recognized from a final voice transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VoiceApprovalIntent {
    /// Approve once. Requires a spoken confirmation before it is applied.
    Approve,
    /// Approve for the rest of the session. Requires a spoken confirmation.
    ApproveForSession,
    /// Decline the request and let the turn continue. Applied immediately.
    Deny,
    /// Decline and stop the turn. Applied immediately.
    Abort,
    /// Confirms a pending voice approval.
    Confirm,
}

const LEADING_FILLERS: &[&str] = &["ok", "okay", "um", "uh", "hmm", "please", "codex", "so"];
const TRAILING_FILLERS: &[&str] = &["please", "thanks", "codex", "now"];
/// Negations and hedges that disqualify an otherwise matching approval.
const NEGATIONS: &[&str] = &[
    "not",
    "don't",
    "dont",
    "never",
    "can't",
    "cant",
    "cannot",
    "won't",
    "wont",
    "shouldn't",
    "shouldnt",
    "wouldn't",
    "wouldnt",
    "isn't",
    "without",
    "but",
    "wait",
    "unless",
    "if",
    "maybe",
];

const APPROVE_PHRASES: &[&str] = &[
    "approve",
    "approved",
    "approve it",
    "approve this",
    "approve that",
    "approve once",
    "approve just once",
    "approve this once",
    "yes",
    "yeah",
    "yep",
    "yes approve",
    "yes approve it",
    "allow",
    "allow it",
    "allow once",
    "allow this once",
    "yes allow",
    "yes allow it",
    "go ahead",
    "yes go ahead",
    "go ahead and approve",
];

const APPROVE_FOR_SESSION_PHRASES: &[&str] = &[
    "approve for session",
    "approve for this session",
    "approve for the session",
    "approve it for this session",
    "approve it for the session",
    "allow for session",
    "allow for this session",
    "allow for the session",
    "allow it for this session",
    "always allow",
    "always allow it",
    "always approve",
    "yes always allow",
    "yes approve for this session",
];

const DENY_PHRASES: &[&str] = &[
    "deny",
    "denied",
    "deny it",
    "deny this",
    "deny that",
    "no",
    "nope",
    "no deny",
    "no deny it",
    "reject",
    "reject it",
    "reject this",
    "decline",
    "decline it",
    "no thanks",
    "no thank you",
];

const ABORT_PHRASES: &[&str] = &[
    "stop",
    "stop it",
    "stop this",
    "cancel",
    "cancel it",
    "cancel this",
    "abort",
    "abort it",
    "no stop",
];

const CONFIRM_PHRASES: &[&str] = &[
    "confirm",
    "confirmed",
    "confirm it",
    "confirm that",
    "yes confirm",
    "yes confirmed",
    "i confirm",
];

/// Lowercase, strip punctuation, and split an utterance into words.
fn normalized_words(utterance: &str) -> Vec<String> {
    let lowered = utterance
        .to_lowercase()
        .replace(['\u{2019}', '\u{2018}'], "'");
    lowered
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '\''))
        .map(|word| word.trim_matches('\''))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse a final user transcript as an approval answer.
///
/// Returns `None` for anything that is not unmistakably an approval answer,
/// including long utterances and any negated phrasing.
pub(crate) fn parse_voice_approval_intent(utterance: &str) -> Option<VoiceApprovalIntent> {
    let mut words = normalized_words(utterance);
    if words.is_empty() || words.len() > MAX_INTENT_WORDS {
        return None;
    }
    while words.len() > 1 && LEADING_FILLERS.contains(&words[0].as_str()) {
        words.remove(0);
    }
    while words.len() > 1
        && words
            .last()
            .is_some_and(|word| TRAILING_FILLERS.contains(&word.as_str()))
    {
        words.pop();
    }
    if words.len() > 2 && words.ends_with(&["thank".to_string(), "you".to_string()]) {
        words.truncate(words.len() - 2);
    }
    let phrase = words.join(" ");
    // Refusals are matched first: "no" and "no thanks" are the negation themselves.
    if DENY_PHRASES.contains(&phrase.as_str()) {
        return Some(VoiceApprovalIntent::Deny);
    }
    if ABORT_PHRASES.contains(&phrase.as_str()) {
        return Some(VoiceApprovalIntent::Abort);
    }
    if words.iter().any(|word| NEGATIONS.contains(&word.as_str())) {
        return None;
    }
    if CONFIRM_PHRASES.contains(&phrase.as_str()) {
        return Some(VoiceApprovalIntent::Confirm);
    }
    if APPROVE_FOR_SESSION_PHRASES.contains(&phrase.as_str()) {
        return Some(VoiceApprovalIntent::ApproveForSession);
    }
    if APPROVE_PHRASES.contains(&phrase.as_str()) {
        return Some(VoiceApprovalIntent::Approve);
    }
    None
}

/// Utterances longer than this are conversation, not an option choice.
const MAX_OPTION_WORDS: usize = 10;
/// Words that may lead into a choice: "okay, I'll go with option 4".
const OPTION_LEAD_INS: &[&str] = &[
    "ok", "okay", "um", "uh", "hmm", "so", "please", "codex", "claude", "i", "i'll", "i'd",
    "let's", "lets", "want", "like", "choose", "pick", "select", "press", "hit", "type", "take",
    "go", "with", "use", "the",
];
const OPTION_TRAILERS: &[&str] = &["please", "thanks", "now", "then", "codex", "claude"];
/// Words that name an option by its position or letter: "option 4", "letter d".
const OPTION_PREFIXES: &[&str] = &["option", "number", "choice", "letter", "key", "button"];
/// Words after an ordinal: "the fourth one", "fourth option".
const OPTION_SUFFIXES: &[&str] = &["one", "option", "choice"];
const NUMBER_WORDS: &[&str] = &[
    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
];
const ORDINAL_WORDS: &[&str] = &[
    "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
];
/// Words a transcriber writes for a spoken number or letter, accepted only
/// after "option" and the like: "option to", "option dee".
const SOUNDALIKES: &[(&str, &str)] = &[
    ("won", "1"),
    ("to", "2"),
    ("too", "2"),
    ("for", "4"),
    ("ay", "a"),
    ("bee", "b"),
    ("see", "c"),
    ("sea", "c"),
    ("dee", "d"),
    ("are", "r"),
    ("why", "y"),
];

/// Position (1-based) named by a number, number word or ordinal: "4", "four",
/// "4th", "fourth".
fn option_number(word: &str) -> Option<usize> {
    let digits = word
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(word.len());
    if digits > 0 && matches!(&word[digits..], "" | "st" | "nd" | "rd" | "th") {
        return word[..digits].parse().ok();
    }
    NUMBER_WORDS
        .iter()
        .chain(ORDINAL_WORDS)
        .position(|name| *name == word)
        .map(|index| index % NUMBER_WORDS.len() + 1)
}

/// The option an explicit reference names: a number, an ordinal, "last", or
/// one of the option's letters.
fn option_by_reference(words: &[&str], options: &[VoiceApprovalOption]) -> Option<usize> {
    let (word, prefixed) = match words {
        [word] => (*word, false),
        [prefix, word] if OPTION_PREFIXES.contains(prefix) => (*word, true),
        ["option", "number", word] => (*word, true),
        [word, suffix] if OPTION_SUFFIXES.contains(suffix) => (*word, false),
        _ => return None,
    };
    let word = if prefixed {
        SOUNDALIKES
            .iter()
            .find(|(heard, _)| *heard == word)
            .map_or(word, |(_, meant)| *meant)
    } else {
        word
    };
    if word == "last" {
        return options.len().checked_sub(1);
    }
    if let Some(number) = option_number(word) {
        return (1..=options.len()).contains(&number).then(|| number - 1);
    }
    let mut letters = word.chars();
    if let (Some(letter), None) = (letters.next(), letters.next())
        && letter.is_ascii_alphabetic()
    {
        return options
            .iter()
            .position(|option| option.keys.contains(&letter));
    }
    None
}

/// Plural-insensitive form of a word, so "permission" matches "permissions".
fn stem(word: &str) -> &str {
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
        &word[..word.len() - 1]
    } else {
        word
    }
}

/// The one option whose label the utterance repeats: all of it, all of it
/// after its leading "yes"/"no", or at least three words in a row of it.
fn option_by_label(words: &[&str], options: &[VoiceApprovalOption]) -> Option<usize> {
    let said: Vec<&str> = words.iter().map(|word| stem(word)).collect();
    let mut matched = options.iter().enumerate().filter(|(_, option)| {
        let label_words = normalized_words(&option.label);
        let label: Vec<&str> = label_words.iter().map(|word| stem(word)).collect();
        let without_answer = match label.first() {
            Some(&"yes" | &"no") => &label[1..],
            _ => &label[..],
        };
        said == label
            || (!without_answer.is_empty() && said == without_answer)
            || (said.len() >= 3 && label.windows(said.len()).any(|window| window == said))
    });
    match (matched.next(), matched.next()) {
        (Some((index, _)), None) => Some(index),
        _ => None,
    }
}

/// Which option of the visible approval prompt an utterance chooses, if it
/// names one: by number ("option 4", "four", "the fourth one"), by its letter
/// ("option d", "d"), or by repeating its label ("continue without
/// permissions"). Returns its index.
pub(crate) fn parse_voice_approval_option(
    utterance: &str,
    options: &[VoiceApprovalOption],
) -> Option<usize> {
    if options.is_empty() {
        return None;
    }
    let words = normalized_words(utterance);
    if words.is_empty() || words.len() > MAX_OPTION_WORDS {
        return None;
    }
    let mut words: Vec<&str> = words.iter().map(String::as_str).collect();
    while words.len() > 1 && OPTION_LEAD_INS.contains(&words[0]) {
        words.remove(0);
    }
    while words.len() > 1
        && words
            .last()
            .is_some_and(|word| OPTION_TRAILERS.contains(word))
    {
        words.pop();
    }
    option_by_reference(&words, options).or_else(|| option_by_label(&words, options))
}

/// "Option 4, no, continue without permissions" for speech and the transcript.
pub(crate) fn approval_option_phrase(index: usize, option: &VoiceApprovalOption) -> String {
    let label = option.label.replace('`', "");
    limit_words(
        &format!("Option {}, {label}", index + 1),
        MAX_SPOKEN_WORDS - 3,
    )
}

/// Spoken after an option that grants something is chosen; only a following
/// "confirm" applies it.
pub(crate) fn approval_option_confirmation_prompt(
    index: usize,
    option: &VoiceApprovalOption,
) -> String {
    format!(
        "{}? Say confirm.",
        approval_option_phrase(index, option).trim_end_matches(['.', '…'])
    )
}

/// Cap text at `max_words`, marking a cut with an ellipsis.
pub(crate) fn limit_words(text: &str, max_words: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= max_words {
        return words.join(" ");
    }
    format!("{}…", words[..max_words].join(" "))
}

fn limit_summary(text: &str) -> String {
    let limited = limit_words(text, MAX_SUMMARY_WORDS);
    if limited.chars().count() <= MAX_SUMMARY_CHARS {
        return limited;
    }
    let mut truncated: String = limited.chars().take(MAX_SUMMARY_CHARS).collect();
    // Prefer cutting on a word boundary.
    if let Some(space) = truncated.rfind(' ')
        && space > MAX_SUMMARY_CHARS / 2
    {
        truncated.truncate(space);
    }
    format!("{}…", truncated.trim_end_matches('…'))
}

fn file_count(count: usize) -> String {
    if count == 1 {
        "1 file".to_string()
    } else {
        format!("{count} files")
    }
}

/// Short, speakable descriptions of what an approval request would do.
pub(crate) struct ApprovalActionSummary {
    /// Completes "Codex wants to …", for example "run cargo test".
    pub(crate) verb_phrase: String,
    /// Completes "Approve …?", for example "cargo test".
    pub(crate) noun_phrase: String,
}

pub(crate) fn approval_action_summary(request: &ApprovalRequest) -> ApprovalActionSummary {
    let (verb_phrase, noun_phrase) = match request {
        ApprovalRequest::Exec(request) => {
            if request.kind == CommandExecutionApprovalKind::WriteStdin {
                (
                    "send input to a terminal".to_string(),
                    "terminal input".to_string(),
                )
            } else if let Some(network) = request.network_approval_context.as_ref() {
                let host = limit_summary(&network.host);
                (format!("reach {host}"), format!("network access to {host}"))
            } else {
                let command = strip_bash_lc_and_escape(&request.command);
                let mut lines = command
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty());
                match lines.next() {
                    Some(first_line) => {
                        let mut summary = limit_summary(first_line);
                        if lines.next().is_some() && !summary.ends_with('…') {
                            summary.push('…');
                        }
                        (format!("run {summary}"), summary)
                    }
                    None => ("run a command".to_string(), "a command".to_string()),
                }
            }
        }
        // A request from another machine arrives without its diff.
        ApprovalRequest::ApplyPatch(request) if request.changes.is_empty() => {
            ("edit files".to_string(), "file edits".to_string())
        }
        ApprovalRequest::ApplyPatch(request) => {
            let count = request.changes.len();
            let target = if count == 1
                && let Some(name) = request
                    .changes
                    .keys()
                    .next()
                    .and_then(|path| path.file_name())
            {
                limit_summary(&name.to_string_lossy())
            } else {
                file_count(count)
            };
            (format!("edit {target}"), format!("edits to {target}"))
        }
        ApprovalRequest::Permissions(_) => (
            "use extra permissions".to_string(),
            "extra permissions".to_string(),
        ),
        ApprovalRequest::McpElicitation(request) => {
            let server = limit_summary(&request.server_name);
            (
                format!("answer a request from {server}"),
                format!("the {server} request"),
            )
        }
    };
    ApprovalActionSummary {
        verb_phrase,
        noun_phrase,
    }
}

fn end_sentence(text: &str) -> String {
    if text.ends_with(['…', '.', '!', '?']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

/// "build on devbox" for a request from an agent on another machine.
fn remote_agent_phrase(request: &ApprovalRequest) -> Option<String> {
    let (machine, agent) = request.remote_origin()?;
    Some(format!(
        "{} on {}",
        limit_summary(agent),
        limit_summary(machine)
    ))
}

fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Spoken when an approval prompt appears during a voice conversation. A
/// request from another machine names its agent and machine: "Build on devbox
/// wants to run cargo test. Say approve or deny."
pub(crate) fn approval_request_prompt(request: &ApprovalRequest) -> String {
    let subject = remote_agent_phrase(request)
        .map(|phrase| capitalize_first(&phrase))
        .unwrap_or_else(|| {
            crate::agent_backend::spoken_agent_name(crate::agent_backend::is_claude_code())
                .to_string()
        });
    limit_words(
        &format!(
            "{subject} wants to {} Say approve or deny.",
            end_sentence(&approval_action_summary(request).verb_phrase)
        ),
        MAX_SPOKEN_WORDS,
    )
}

/// Spoken after an approve intent; only a following "confirm" applies it.
pub(crate) fn approval_confirmation_prompt(
    request: &ApprovalRequest,
    intent: VoiceApprovalIntent,
) -> String {
    let scope = if intent == VoiceApprovalIntent::ApproveForSession {
        " for this session"
    } else {
        ""
    };
    let target = approval_action_summary(request).noun_phrase;
    let origin = remote_agent_phrase(request)
        .map(|phrase| format!(" for {phrase}"))
        .unwrap_or_default();
    limit_words(
        &format!("Approve {target}{origin}{scope}? Say confirm."),
        MAX_SPOKEN_WORDS,
    )
}

/// The text up to the first `.`, `!` or `?` that ends a sentence, meaning one
/// followed by whitespace or the end of the text. Dots inside model names,
/// versions, file names and URLs (`gpt-5.1-codex`, `config.toml`) don't count.
fn first_sentence(text: &str) -> &str {
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if matches!(ch, '.' | '!' | '?')
            && chars.peek().is_none_or(|(_, next)| next.is_whitespace())
        {
            return &text[..index + ch.len_utf8()];
        }
    }
    text
}

/// Spoken when a turn fails during a voice conversation.
pub(crate) fn turn_error_speech(message: &str) -> String {
    let first_line = message.lines().find(|line| !line.trim().is_empty());
    let Some(first_line) = first_line else {
        return "Codex hit an error.".to_string();
    };
    let sentence = first_sentence(first_line)
        .trim()
        .trim_end_matches(['.', '!', '?']);
    limit_words(
        &format!(
            "Codex hit an error: {}.",
            limit_words(sentence, MAX_ERROR_WORDS).trim_end_matches('…')
        ),
        MAX_SPOKEN_WORDS,
    )
}

#[cfg(test)]
#[path = "voice_approval_tests.rs"]
mod tests;
