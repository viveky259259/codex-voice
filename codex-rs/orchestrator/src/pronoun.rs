//! "Tell it to ...", "stop it", "what's it doing": the last agent.
//!
//! The rewrite runs only after the normal parse found no command, so a named
//! agent always wins. The first pronoun phrase in the transcript is replaced
//! with the last agent's name, and the result is parsed against a context
//! holding only that agent. It counts only if the parser then sees an agent
//! command (status, steer, or stop); any other shape is not a pronoun
//! reference, so "tell me how it works" stays a prompt.
//!
//! Precedence with Lane A's approval grammar: while an approval prompt is
//! open, a bare "stop" and "stop it/this/that" belong to the approval, so a
//! pronoun stop is dropped here (see [`crate::Orchestrator::parse`]).

use codex_voice_commands::AgentRef;
use codex_voice_commands::VoiceCommand;
use codex_voice_commands::VoiceContext;
use codex_voice_commands::parse_voice_command;

/// Phrases that refer back to the last agent, longest first.
const PRONOUN_PHRASES: &[&[&str]] = &[
    &["the", "same", "agent"],
    &["that", "same", "agent"],
    &["same", "agent"],
    &["that", "agent"],
    &["this", "agent"],
    &["that", "one"],
    &["it"],
];

/// One word of the transcript with its byte span.
struct Span {
    text: String,
    start: usize,
    end: usize,
}

fn words(transcript: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut start = None;
    let ends = transcript
        .char_indices()
        .chain(std::iter::once((transcript.len(), ' ')));
    for (index, ch) in ends {
        if ch.is_alphanumeric() || matches!(ch, '\'' | '\u{2019}') {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            spans.push(Span {
                text: transcript[begin..index]
                    .to_lowercase()
                    .replace('\u{2019}', "'"),
                start: begin,
                end: index,
            });
        }
    }
    spans
}

/// The byte range of the first pronoun phrase, if any.
fn first_pronoun(transcript: &str) -> Option<(usize, usize)> {
    let spans = words(transcript);
    (0..spans.len()).find_map(|index| {
        PRONOUN_PHRASES.iter().find_map(|phrase| {
            let window = spans.get(index..index + phrase.len())?;
            let matches = window
                .iter()
                .zip(phrase.iter())
                .all(|(span, want)| span.text == *want);
            matches.then(|| (window[0].start, window[window.len() - 1].end))
        })
    })
}

/// Resolve a pronoun reference to `last`, or `None` when the transcript is
/// not an agent command about it.
pub(crate) fn resolve_pronoun(transcript: &str, last: &AgentRef) -> Option<VoiceCommand> {
    let (start, end) = first_pronoun(transcript)?;
    let rewritten = format!(
        "{}{}{}",
        &transcript[..start],
        last.name,
        &transcript[end..]
    );
    let context = VoiceContext {
        known_agents: vec![last.clone()],
        ..VoiceContext::default()
    };
    match parse_voice_command(&rewritten, &context) {
        command @ (VoiceCommand::AgentStatus { .. }
        | VoiceCommand::SteerAgent { .. }
        | VoiceCommand::StopAgent { .. }) => Some(command),
        VoiceCommand::None
        | VoiceCommand::ListAgents
        | VoiceCommand::SwitchModel { .. }
        | VoiceCommand::ListMachines
        | VoiceCommand::AttachMachine { .. }
        | VoiceCommand::Ambiguous { .. }
        | VoiceCommand::NotFound { .. } => None,
    }
}

#[cfg(test)]
#[path = "pronoun_tests.rs"]
mod tests;
