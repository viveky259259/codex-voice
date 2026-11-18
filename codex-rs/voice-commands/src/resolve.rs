//! Fuzzy, conservative resolution of spoken names against the context.
//!
//! Two tiers: an exact match on a separator-free key ("mac studio" ==
//! "MacStudio" == "mac-studio") wins outright; otherwise, where allowed, a
//! phrase whose words all appear in one name ("frontend" in "build-frontend")
//! matches. More than one match is reported, never guessed.

use crate::AgentRef;
use crate::Candidate;
use crate::ModelRef;
use crate::text::key;
use crate::text::name_words;

/// Leading words that never belong to a name.
pub(crate) const DETERMINERS: &[&str] = &["my", "the", "our"];
const AGENT_NOUNS: &[&str] = &["agent"];
const MACHINE_NOUNS: &[&str] = &["machine", "computer", "box"];
const MODEL_NOUNS: &[&str] = &["model"];
/// Words too generic to select anything by partial match on their own.
const GENERIC_WORDS: &[&str] = &[
    "agent",
    "agents",
    "machine",
    "computer",
    "model",
    "server",
    "task",
    "thread",
    "session",
    "it",
    "this",
    "that",
    "1",
    "all",
    "everything",
    "on",
];

pub(crate) fn strip_leading<'a>(mut words: &'a [String], drop: &[&str]) -> &'a [String] {
    while let Some((first, rest)) = words.split_first()
        && drop.contains(&first.as_str())
    {
        words = rest;
    }
    words
}

pub(crate) fn strip_trailing<'a>(mut words: &'a [String], drop: &[&str]) -> &'a [String] {
    while let Some((last, rest)) = words.split_last()
        && drop.contains(&last.as_str())
    {
        words = rest;
    }
    words
}

fn strip_noun<'a>(words: &'a [String], nouns: &[&str]) -> &'a [String] {
    strip_trailing(strip_leading(words, nouns), nouns)
}

/// Each item's names as normalized words.
type Names = Vec<Vec<Vec<String>>>;

fn names_of<'a>(items: impl IntoIterator<Item = &'a str>) -> Names {
    items
        .into_iter()
        .map(|name| vec![name_words(name)])
        .collect()
}

fn exact_matches(variants: &[&[String]], names: &Names) -> Vec<usize> {
    let keys: Vec<String> = variants
        .iter()
        .filter(|variant| !variant.is_empty())
        .map(|variant| key(variant))
        .collect();
    names
        .iter()
        .enumerate()
        .filter(|(_, item_names)| {
            item_names
                .iter()
                .any(|name| !name.is_empty() && keys.contains(&key(name)))
        })
        .map(|(index, _)| index)
        .collect()
}

fn partial_matches(phrase: &[String], names: &Names) -> Vec<usize> {
    let specific = phrase.iter().any(|word| {
        word.chars().any(char::is_alphabetic) && !GENERIC_WORDS.contains(&word.as_str())
    });
    if !specific {
        return Vec::new();
    }
    names
        .iter()
        .enumerate()
        .filter(|(_, item_names)| {
            item_names
                .iter()
                .any(|name| phrase.iter().all(|word| name.contains(word)))
        })
        .map(|(index, _)| index)
        .collect()
}

/// Exact matches over every variant, else partial matches on the last one.
fn match_names(variants: &[&[String]], names: &Names, allow_partial: bool) -> Vec<usize> {
    let exact = exact_matches(variants, names);
    if !exact.is_empty() || !allow_partial {
        return exact;
    }
    variants
        .last()
        .map(|phrase| partial_matches(phrase, names))
        .unwrap_or_default()
}

/// Indices of `agents` that `phrase` refers to, by name or by machine.
///
/// Understands "build", "the build agent", "the desktop agent", "the agent on
/// my desktop", and "build on laptop".
pub(crate) fn resolve_agents(phrase: &[String], agents: &[AgentRef]) -> Vec<usize> {
    let phrase = strip_leading(phrase, DETERMINERS);
    if phrase.is_empty() {
        return Vec::new();
    }
    let machines = distinct_machines(agents.iter().filter_map(|agent| agent.machine.as_deref()));
    let machine_names = names_of(machines.iter().map(String::as_str));
    let agents_on = |machine_indices: &[usize]| -> Vec<usize> {
        let keys: Vec<String> = machine_indices
            .iter()
            .map(|index| key(&name_words(&machines[*index])))
            .collect();
        agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| {
                agent
                    .machine
                    .as_deref()
                    .is_some_and(|machine| keys.contains(&key(&name_words(machine))))
            })
            .map(|(index, _)| index)
            .collect()
    };

    // "<name> on <machine>" and "the agent (running) on <machine>".
    if let Some(on) = phrase.iter().rposition(|word| word == "on")
        && on + 1 < phrase.len()
    {
        let machine_phrase = strip_leading(&phrase[on + 1..], DETERMINERS);
        let machine_matches = match_names(
            &[machine_phrase, strip_noun(machine_phrase, MACHINE_NOUNS)],
            &machine_names,
            /*allow_partial*/ true,
        );
        if !machine_matches.is_empty() {
            let on_machine = agents_on(&machine_matches);
            let left = strip_trailing(&phrase[..on], &["running"]);
            let left = strip_noun(strip_leading(left, DETERMINERS), AGENT_NOUNS);
            if left.is_empty() {
                return on_machine;
            }
            let names = names_of(on_machine.iter().map(|index| agents[*index].name.as_str()));
            return match_names(&[left], &names, /*allow_partial*/ true)
                .into_iter()
                .map(|index| on_machine[index])
                .collect();
        }
    }

    let core = strip_noun(phrase, AGENT_NOUNS);
    if core.is_empty() {
        // "the agent": every agent is a candidate.
        return (0..agents.len()).collect();
    }
    let variants = [phrase, core];
    let agent_names = names_of(agents.iter().map(|agent| agent.name.as_str()));
    let machine_variants = [phrase, core, strip_noun(core, MACHINE_NOUNS)];
    let mut matched = exact_matches(&variants, &agent_names);
    matched.extend(agents_on(&exact_matches(&machine_variants, &machine_names)));
    if matched.is_empty() {
        matched.extend(partial_matches(core, &agent_names));
        matched.extend(agents_on(&partial_matches(core, &machine_names)));
    }
    matched.sort_unstable();
    matched.dedup();
    matched
}

/// Indices of `models` that `phrase` refers to, by slug or spoken alias.
pub(crate) fn resolve_models(phrase: &[String], models: &[ModelRef]) -> Vec<usize> {
    let phrase = strip_leading(phrase, DETERMINERS);
    let names: Names = models
        .iter()
        .map(|model| {
            std::iter::once(model.slug.as_str())
                .chain(model.spoken_aliases.iter().map(String::as_str))
                .map(name_words)
                .collect()
        })
        .collect();
    match_names(
        &[phrase, strip_noun(phrase, MODEL_NOUNS)],
        &names,
        /*allow_partial*/ true,
    )
}

/// Indices of `machines` that `phrase` names exactly (up to separators).
///
/// No partial matching: "connect to the database" must not pick a machine
/// called "database-host".
pub(crate) fn resolve_machines(phrase: &[String], machines: &[String]) -> Vec<usize> {
    let phrase = strip_leading(phrase, DETERMINERS);
    match_names(
        &[phrase, strip_noun(phrase, MACHINE_NOUNS)],
        &names_of(machines.iter().map(String::as_str)),
        /*allow_partial*/ false,
    )
}

/// Indices of `candidates` that `phrase` names by label or id.
pub(crate) fn resolve_candidates(phrase: &[String], candidates: &[Candidate]) -> Vec<usize> {
    let names: Names = candidates
        .iter()
        .map(|candidate| vec![name_words(&candidate.label), name_words(&candidate.id)])
        .collect();
    match_names(&[phrase], &names, /*allow_partial*/ true)
}

/// Machine names deduplicated by key, first spelling wins.
pub(crate) fn distinct_machines<'a>(machines: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = Vec::new();
    let mut distinct = Vec::new();
    for machine in machines {
        let machine_key = key(&name_words(machine));
        if machine_key.is_empty() || seen.contains(&machine_key) {
            continue;
        }
        seen.push(machine_key);
        distinct.push(machine.to_string());
    }
    distinct
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
