//! The spoken command grammar.
//!
//! An utterance is a command only when, after dropping a few fillers ("ok",
//! "hey codex", "can you", "please"), it starts with one of the fixed shapes
//! below and its target resolves against the [`VoiceContext`]:
//!
//! | Command | Shapes |
//! |---|---|
//! | `ListAgents` | "what's running", "list (my) agents", "show (me) my agents" |
//! | `AgentStatus` | "what's X doing", "what is X working on", "status of X", "check on X" |
//! | `SteerAgent` | "tell X to ...", "ask X to ..." |
//! | `StopAgent` | "stop X", "interrupt X" (never a bare "stop") |
//! | `SwitchModel` | "switch to M", "use M", "switch the model to M" |
//! | `ListMachines` | "list my machines", "show my machines" |
//! | `AttachMachine` | "connect to H", "attach to H", "ssh into H" |

use crate::AgentRef;
use crate::Candidate;
use crate::VoiceAction;
use crate::VoiceCommand;
use crate::VoiceContext;
use crate::resolve::DETERMINERS;
use crate::resolve::distinct_machines;
use crate::resolve::resolve_agents;
use crate::resolve::resolve_candidates;
use crate::resolve::resolve_machines;
use crate::resolve::resolve_models;
use crate::resolve::strip_leading;
use crate::resolve::strip_trailing;
use crate::speakable;
use crate::text::Word;
use crate::text::normalize;
use crate::text::spoken_words;

/// Commands other than steering are short; longer speech is a prompt.
const MAX_COMMAND_WORDS: usize = 12;
/// Confirmation answers are short, as in the approval grammar.
const MAX_CONFIRMATION_WORDS: usize = 6;

const LEADING_FILLERS: &[&[&str]] = &[
    &["ok"],
    &["okay"],
    &["alright"],
    &["um"],
    &["uh"],
    &["hmm"],
    &["hey"],
    &["so"],
    &["codex"],
    &["please"],
    &["can", "you"],
    &["could", "you"],
    &["would", "you"],
    &["will", "you"],
];
const TRAILING_FILLERS: &[&[&str]] = &[
    &["thank", "you"],
    &["right", "now"],
    &["for", "me"],
    &["please"],
    &["thanks"],
    &["now"],
    &["codex"],
];

const LIST_AGENTS_PHRASES: &[&str] = &[
    "what is running",
    "who is running",
    "what agents are running",
    "which agents are running",
    "what are my agents doing",
    "what are the agents doing",
    "list agents",
    "list my agents",
    "list the agents",
    "list all agents",
    "list all my agents",
    "list running agents",
    "show agents",
    "show my agents",
    "show the agents",
    "show all agents",
    "show me my agents",
    "show me the agents",
    "show me all agents",
    "show me all my agents",
    "agent status",
];

const LIST_MACHINES_PHRASES: &[&str] = &[
    "list machines",
    "list my machines",
    "list the machines",
    "list all machines",
    "list all my machines",
    "list my computers",
    "show machines",
    "show my machines",
    "show me my machines",
    "show my computers",
    "what machines do i have",
    "which machines do i have",
    "what machines are online",
    "which machines are online",
];

/// `(prefix, suffix)` around the agent in a status question.
const STATUS_SHAPES: &[(&[&str], &[&str])] = &[
    (&["what", "is"], &["doing"]),
    (&["what", "is"], &["working", "on"]),
    (&["what", "is"], &["up", "to"]),
    (&["how", "is"], &["doing"]),
    (&["how", "is"], &["getting", "on"]),
    (&["what", "is", "the", "status", "of"], &[]),
    (&["what", "is", "the", "status", "for"], &[]),
    (&["give", "me", "the", "status", "of"], &[]),
    (&["status", "of"], &[]),
    (&["status", "for"], &[]),
    (&["check", "on"], &[]),
];

const STOP_VERBS: &[&str] = &["stop", "interrupt"];
/// Lane A's approval grammar owns these after "stop".
const STOP_APPROVAL_OBJECTS: &[&str] = &["it", "this", "that", "everything", "all", "now"];

/// `(prefix, names_a_model)` for model switches, longest first.
const SWITCH_MODEL_SHAPES: &[(&[&str], bool)] = &[
    (&["switch", "the", "model", "to"], true),
    (&["change", "the", "model", "to"], true),
    (&["set", "the", "model", "to"], true),
    (&["switch", "models", "to"], true),
    (&["switch", "model", "to"], true),
    (&["change", "model", "to"], true),
    (&["set", "model", "to"], true),
    (&["switch", "to"], false),
    (&["swap", "to"], false),
    (&["use", "model"], true),
    (&["use"], false),
];

const ATTACH_SHAPES: &[&[&str]] = &[
    &["connect", "me", "to"],
    &["attach", "me", "to"],
    &["connect", "to"],
    &["attach", "to"],
    &["ssh", "into"],
    &["ssh", "to"],
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
const NEGATIONS: &[&str] = &[
    "not", "don't", "dont", "never", "can't", "cant", "cannot", "won't", "wont", "but", "wait",
    "unless", "if", "maybe",
];

const ORDINALS: &[&str] = &["first", "second", "third", "fourth", "fifth"];
/// Words in a choice answer that never name a candidate.
const CHOICE_FILLERS: &[&str] = &[
    "the", "my", "one", "agent", "machine", "model", "please", "i", "mean", "meant", "that",
];

fn texts(words: &[Word]) -> Vec<String> {
    words.iter().map(|word| word.text.clone()).collect()
}

fn starts_with(words: &[String], prefix: &[&str]) -> bool {
    words.len() >= prefix.len() && words.iter().zip(prefix).all(|(word, want)| word == want)
}

fn ends_with(words: &[String], suffix: &[&str]) -> bool {
    words.len() >= suffix.len()
        && words[words.len() - suffix.len()..]
            .iter()
            .zip(suffix)
            .all(|(word, want)| word == want)
}

fn strip_leading_fillers(mut words: &[Word]) -> &[Word] {
    loop {
        let current = texts(words);
        let Some(filler) = LEADING_FILLERS
            .iter()
            .find(|filler| filler.len() < words.len() && starts_with(&current, filler))
        else {
            return words;
        };
        words = &words[filler.len()..];
    }
}

fn strip_trailing_fillers(mut words: &[Word]) -> &[Word] {
    loop {
        let current = texts(words);
        let Some(filler) = TRAILING_FILLERS
            .iter()
            .find(|filler| filler.len() < words.len() && ends_with(&current, filler))
        else {
            return words;
        };
        words = &words[..words.len() - filler.len()];
    }
}

fn normalized(words: &[String]) -> Vec<String> {
    normalize(words.iter().map(String::as_str))
}

/// The target as heard, for speech: "the deploy agent" -> "deploy".
fn heard(target: &[String], nouns: &[&str]) -> String {
    let core = strip_leading(target, DETERMINERS);
    let core = strip_trailing(strip_leading(core, nouns), nouns);
    core.join(" ")
}

/// Whether a target explicitly names its kind, as in "the deploy agent".
fn names_kind(target: &[String], nouns: &[&str]) -> bool {
    target
        .last()
        .is_some_and(|last| nouns.contains(&last.as_str()))
        && !heard(target, nouns).is_empty()
}

fn agent_candidates(indices: &[usize], context: &VoiceContext) -> Vec<Candidate> {
    indices
        .iter()
        .map(|index| {
            let agent = &context.known_agents[*index];
            Candidate {
                id: agent.id.clone(),
                label: agent.spoken_label(),
                machine: agent.machine.clone(),
            }
        })
        .collect()
}

/// Resolve an agent target and build the command, or report why not.
fn agent_command(
    target: &[String],
    action: VoiceAction,
    context: &VoiceContext,
    build: impl FnOnce(AgentRef) -> VoiceCommand,
) -> Option<VoiceCommand> {
    let matches = resolve_agents(&normalized(target), &context.known_agents);
    match matches.as_slice() {
        [] => names_kind(target, &["agent"]).then(|| VoiceCommand::NotFound {
            action,
            heard: heard(target, &["agent"]),
        }),
        [index] => Some(build(context.known_agents[*index].clone())),
        indices => Some(VoiceCommand::Ambiguous {
            action,
            heard: heard(target, &["agent"]),
            candidates: agent_candidates(indices, context),
        }),
    }
}

/// Clean the spoken steering message: trim and drop a final period.
fn clean_message(raw: &str) -> String {
    raw.trim()
        .trim_end_matches(['.', ',', ';'])
        .trim_end()
        .to_string()
}

/// "tell X to ..." / "ask X to ...".
///
/// Agent names may contain "to", so every "to" is a possible split; the
/// longest target that resolves wins.
fn parse_steer(transcript: &str, body: &[Word], context: &VoiceContext) -> VoiceCommand {
    let rest = &body[1..];
    let rest_texts = texts(rest);
    // (target, message) for each "to" that has a non-empty message after it.
    let splits: Vec<(&[String], String)> = rest
        .iter()
        .enumerate()
        .filter(|(index, word)| *index > 0 && word.text == "to")
        .filter_map(|(index, _)| {
            let message = clean_message(&transcript[rest.get(index + 1)?.start..]);
            (!message.is_empty()).then_some((&rest_texts[..index], message))
        })
        .collect();
    for (target, message) in splits.iter().rev() {
        let matches = resolve_agents(&normalized(target), &context.known_agents);
        match matches.as_slice() {
            [] => continue,
            [agent] => {
                return VoiceCommand::SteerAgent {
                    agent: context.known_agents[*agent].clone(),
                    message: message.clone(),
                    needs_confirmation: true,
                };
            }
            indices => {
                return VoiceCommand::Ambiguous {
                    action: VoiceAction::SteerAgent {
                        message: message.clone(),
                    },
                    heard: heard(target, &["agent"]),
                    candidates: agent_candidates(indices, context),
                };
            }
        }
    }
    match splits.first() {
        Some((target, message)) if names_kind(target, &["agent"]) => VoiceCommand::NotFound {
            action: VoiceAction::SteerAgent {
                message: message.clone(),
            },
            heard: heard(target, &["agent"]),
        },
        _ => VoiceCommand::None,
    }
}

fn parse_stop(words: &[String], context: &VoiceContext) -> Option<VoiceCommand> {
    let (verb, target) = words.split_first()?;
    if !STOP_VERBS.contains(&verb.as_str())
        || target.is_empty()
        || (target.len() == 1 && STOP_APPROVAL_OBJECTS.contains(&target[0].as_str()))
    {
        return None;
    }
    agent_command(target, VoiceAction::StopAgent, context, |agent| {
        VoiceCommand::StopAgent {
            agent,
            needs_confirmation: true,
        }
    })
}

fn parse_status(words: &[String], context: &VoiceContext) -> Option<VoiceCommand> {
    STATUS_SHAPES.iter().find_map(|(prefix, suffix)| {
        if words.len() <= prefix.len() + suffix.len()
            || !starts_with(words, prefix)
            || !ends_with(words, suffix)
        {
            return None;
        }
        let target = &words[prefix.len()..words.len() - suffix.len()];
        agent_command(target, VoiceAction::AgentStatus, context, |agent| {
            VoiceCommand::AgentStatus { agent }
        })
    })
}

fn parse_switch_model(words: &[String], context: &VoiceContext) -> Option<VoiceCommand> {
    let (prefix, names_model) = SWITCH_MODEL_SHAPES
        .iter()
        .find(|(prefix, _)| words.len() > prefix.len() && starts_with(words, prefix))?;
    let target = &words[prefix.len()..];
    let models = &context.known_models;
    let matches = resolve_models(&normalized(target), models);
    match matches.as_slice() {
        [] => (*names_model || names_kind(target, &["model"])).then(|| VoiceCommand::NotFound {
            action: VoiceAction::SwitchModel,
            heard: heard(target, &["model"]),
        }),
        [index] => Some(VoiceCommand::SwitchModel {
            slug: models[*index].slug.clone(),
        }),
        indices => Some(VoiceCommand::Ambiguous {
            action: VoiceAction::SwitchModel,
            heard: heard(target, &["model"]),
            candidates: indices
                .iter()
                .map(|index| Candidate {
                    id: models[*index].slug.clone(),
                    label: speakable(&models[*index].slug),
                    machine: None,
                })
                .collect(),
        }),
    }
}

/// Every machine the context knows, including the machines agents run on.
fn all_machines(context: &VoiceContext) -> Vec<String> {
    distinct_machines(
        context.known_machines.iter().map(String::as_str).chain(
            context
                .known_agents
                .iter()
                .filter_map(|agent| agent.machine.as_deref()),
        ),
    )
}

fn parse_attach(words: &[String], context: &VoiceContext) -> Option<VoiceCommand> {
    let prefix = ATTACH_SHAPES
        .iter()
        .find(|prefix| words.len() > prefix.len() && starts_with(words, prefix))?;
    let target = &words[prefix.len()..];
    let machines = all_machines(context);
    let matches = resolve_machines(&normalized(target), &machines);
    let nouns = ["machine", "computer"];
    match matches.as_slice() {
        [] => names_kind(target, &nouns).then(|| VoiceCommand::NotFound {
            action: VoiceAction::AttachMachine,
            heard: heard(target, &nouns),
        }),
        [index] => Some(VoiceCommand::AttachMachine {
            machine: machines[*index].clone(),
        }),
        indices => Some(VoiceCommand::Ambiguous {
            action: VoiceAction::AttachMachine,
            heard: heard(target, &nouns),
            candidates: indices
                .iter()
                .map(|index| Candidate {
                    id: machines[*index].clone(),
                    label: speakable(&machines[*index]),
                    machine: None,
                })
                .collect(),
        }),
    }
}

/// Parse a final voice transcript.
///
/// Returns [`VoiceCommand::None`] for anything that is not unmistakably a
/// command with a known target, so ordinary coding requests reach the model.
pub fn parse_voice_command(transcript: &str, context: &VoiceContext) -> VoiceCommand {
    let words = spoken_words(transcript);
    let body = strip_leading_fillers(&words);
    let Some(first) = body.first() else {
        return VoiceCommand::None;
    };
    if matches!(first.text.as_str(), "tell" | "ask") {
        return parse_steer(transcript, body, context);
    }
    let body = strip_trailing_fillers(body);
    if body.len() > MAX_COMMAND_WORDS {
        return VoiceCommand::None;
    }
    let words = texts(body);
    let phrase = words.join(" ");
    if LIST_AGENTS_PHRASES.contains(&phrase.as_str()) {
        return VoiceCommand::ListAgents;
    }
    if LIST_MACHINES_PHRASES.contains(&phrase.as_str()) {
        return VoiceCommand::ListMachines;
    }
    parse_stop(&words, context)
        .or_else(|| parse_status(&words, context))
        .or_else(|| parse_switch_model(&words, context))
        .or_else(|| parse_attach(&words, context))
        .unwrap_or_default()
}

/// Whether an utterance confirms a pending destructive command.
///
/// Mirrors the approval grammar: short, exact phrases only, and any negation
/// or hedge ("wait", "don't", "maybe") disqualifies it.
pub fn is_voice_confirmation(utterance: &str) -> bool {
    let words = spoken_words(utterance);
    if words.is_empty() || words.len() > MAX_CONFIRMATION_WORDS {
        return false;
    }
    let body = strip_trailing_fillers(strip_leading_fillers(&words));
    let words = texts(body);
    if words.iter().any(|word| NEGATIONS.contains(&word.as_str())) {
        return false;
    }
    CONFIRM_PHRASES.contains(&words.join(" ").as_str())
}

/// Answer an [`VoiceCommand::Ambiguous`] prompt.
///
/// Accepts an ordinal ("the first one", "number two") or enough of a label
/// to pick exactly one candidate ("the one on laptop"). Returns `None` when
/// the answer does not pick exactly one; the caller then drops the command.
pub fn resolve_choice(
    action: &VoiceAction,
    candidates: &[Candidate],
    utterance: &str,
    context: &VoiceContext,
) -> Option<VoiceCommand> {
    let words = spoken_words(utterance);
    let words = texts(strip_trailing_fillers(strip_leading_fillers(&words)));
    if words.is_empty() || words.len() > MAX_COMMAND_WORDS {
        return None;
    }
    let ordinal = words
        .iter()
        .find_map(|word| ORDINALS.iter().position(|ordinal| ordinal == word))
        .or_else(|| match normalized(&words).as_slice() {
            [number, digit] if matches!(number.as_str(), "number" | "option") => {
                digit.parse::<usize>().ok()?.checked_sub(1)
            }
            _ => None,
        });
    let chosen = match ordinal {
        Some(index) => candidates.get(index)?,
        None => {
            let phrase: Vec<String> = words
                .into_iter()
                .filter(|word| !CHOICE_FILLERS.contains(&word.as_str()))
                .collect();
            match resolve_candidates(&normalized(&phrase), candidates).as_slice() {
                [index] => &candidates[*index],
                _ => return None,
            }
        }
    };
    let agent = || {
        context
            .known_agents
            .iter()
            .find(|agent| agent.id == chosen.id && agent.machine == chosen.machine)
            .cloned()
    };
    Some(match action {
        VoiceAction::AgentStatus => VoiceCommand::AgentStatus { agent: agent()? },
        VoiceAction::SteerAgent { message } => VoiceCommand::SteerAgent {
            agent: agent()?,
            message: message.clone(),
            needs_confirmation: true,
        },
        VoiceAction::StopAgent => VoiceCommand::StopAgent {
            agent: agent()?,
            needs_confirmation: true,
        },
        VoiceAction::SwitchModel => VoiceCommand::SwitchModel {
            slug: chosen.id.clone(),
        },
        VoiceAction::AttachMachine => VoiceCommand::AttachMachine {
            machine: chosen.id.clone(),
        },
    })
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
