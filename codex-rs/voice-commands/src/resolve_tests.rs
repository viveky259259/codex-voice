use super::*;
use crate::test_fixtures::agent;
use crate::test_fixtures::context;
use crate::test_fixtures::twin_builds;
use crate::text::normalize;
use pretty_assertions::assert_eq;

fn phrase(text: &str) -> Vec<String> {
    normalize(text.split_whitespace())
}

fn agents(text: &str) -> Vec<usize> {
    resolve_agents(&phrase(text), &context().known_agents)
}

#[test]
fn agents_resolve_by_name_with_determiners_and_noun() {
    assert_eq!(agents("build"), vec![0]);
    assert_eq!(agents("BUILD"), vec![0]);
    assert_eq!(agents("the build agent"), vec![0]);
    assert_eq!(agents("my tests"), vec![1]);
    assert_eq!(agents("agent tests"), vec![1]);
}

#[test]
fn agents_resolve_spoken_separators() {
    assert_eq!(agents("docs writer"), vec![2]);
    assert_eq!(agents("docswriter"), vec![2]);
    // A unique word of a name is enough.
    assert_eq!(agents("writer"), vec![2]);
}

#[test]
fn agents_resolve_by_machine() {
    assert_eq!(agents("desktop"), vec![0]);
    assert_eq!(agents("the desktop agent"), vec![0]);
    assert_eq!(agents("the agent on my laptop"), vec![1]);
    assert_eq!(agents("agent running on the laptop"), vec![1]);
    assert_eq!(agents("mac studio"), vec![2]);
    assert_eq!(agents("the macstudio agent"), vec![2]);
    assert_eq!(agents("my mac studio machine"), vec![2]);
}

#[test]
fn agents_resolve_name_on_machine() {
    let twins = twin_builds().known_agents;
    assert_eq!(resolve_agents(&phrase("build"), &twins), vec![0, 1]);
    assert_eq!(resolve_agents(&phrase("build on laptop"), &twins), vec![1]);
    assert_eq!(
        resolve_agents(&phrase("the build agent on the desktop"), &twins),
        vec![0]
    );
    assert_eq!(resolve_agents(&phrase("desktop"), &twins), vec![0, 2]);
    assert_eq!(
        resolve_agents(&phrase("lint on laptop"), &twins),
        Vec::<usize>::new()
    );
}

#[test]
fn bare_agent_means_every_agent() {
    assert_eq!(agents("the agent"), vec![0, 1, 2]);
    assert_eq!(
        resolve_agents(&phrase("the agent"), &[]),
        Vec::<usize>::new()
    );
}

#[test]
fn agent_names_with_numbers_match_spoken_numbers() {
    let workers = vec![agent("w1", "worker-1", None), agent("w2", "worker_2", None)];
    assert_eq!(resolve_agents(&phrase("worker two"), &workers), vec![1]);
    assert_eq!(resolve_agents(&phrase("worker 1"), &workers), vec![0]);
    // "worker" alone is ambiguous, never guessed.
    assert_eq!(resolve_agents(&phrase("worker"), &workers), vec![0, 1]);
}

#[test]
fn unrelated_phrases_resolve_to_nothing() {
    for text in [
        "using unwrap in parser rs",
        "this function",
        "it",
        "the server",
        "me how",
        "",
        "the",
    ] {
        assert_eq!(agents(text), Vec::<usize>::new(), "{text:?}");
    }
}

#[test]
fn generic_words_never_partially_match() {
    let servers = vec![agent("s", "dev-server", None)];
    assert_eq!(
        resolve_agents(&phrase("the server"), &servers),
        Vec::<usize>::new()
    );
    assert_eq!(resolve_agents(&phrase("dev server"), &servers), vec![0]);
    assert_eq!(resolve_agents(&phrase("dev"), &servers), vec![0]);
}

#[test]
fn models_resolve_by_alias_slug_and_words() {
    let models = context().known_models;
    let resolve = |text: &str| resolve_models(&phrase(text), &models);
    assert_eq!(resolve("claude"), vec![0]);
    assert_eq!(resolve("the opus model"), vec![0]);
    assert_eq!(resolve("sonnet"), vec![1]);
    assert_eq!(resolve("gpt five"), vec![2]);
    assert_eq!(resolve("gpt five point five"), vec![2]);
    assert_eq!(resolve("gpt 5.5"), vec![2]);
    assert_eq!(resolve("claude-sonnet-5"), vec![1]);
    assert_eq!(resolve("mini"), vec![3]);
    assert_eq!(resolve("llama"), Vec::<usize>::new());
    // Numbers alone never pick a model.
    assert_eq!(resolve("five"), Vec::<usize>::new());
}

#[test]
fn shared_aliases_are_ambiguous() {
    let models = twin_builds().known_models;
    assert_eq!(resolve_models(&phrase("claude"), &models), vec![0, 1]);
    assert_eq!(resolve_models(&phrase("sonnet"), &models), vec![1]);
}

#[test]
fn machines_need_an_exact_spoken_match() {
    let machines = context().known_machines;
    let resolve = |text: &str| resolve_machines(&phrase(text), &machines);
    assert_eq!(resolve("my mac studio"), vec![2]);
    assert_eq!(resolve("Mac-Studio"), vec![2]);
    assert_eq!(resolve("the desktop machine"), vec![0]);
    assert_eq!(resolve("work box"), vec![3]);
    assert_eq!(resolve("studio"), Vec::<usize>::new());
    assert_eq!(resolve("the database"), Vec::<usize>::new());
}

#[test]
fn distinct_machines_dedupe_by_key() {
    assert_eq!(
        distinct_machines(["MacStudio", "mac-studio", "desktop", ""]),
        vec!["MacStudio".to_string(), "desktop".to_string()]
    );
}
