use super::*;
use crate::VoiceAction;
use crate::parse_voice_command;
use crate::test_fixtures::agent;
use crate::test_fixtures::context;
use crate::test_fixtures::twin_builds;
use pretty_assertions::assert_eq;

fn respond(text: &str) -> Option<String> {
    let context = context();
    spoken_response(&parse_voice_command(text, &context), &context)
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

#[test]
fn list_agents_reads_names_and_machines() {
    let context = VoiceContext {
        known_agents: vec![
            agent("a", "build", Some("desktop")),
            agent("b", "tests", Some("laptop")),
        ],
        ..VoiceContext::default()
    };
    assert_eq!(
        spoken_response(&VoiceCommand::ListAgents, &context),
        Some("Two agents running: build on desktop, tests on laptop.".to_string())
    );
    assert_eq!(
        spoken_response(&VoiceCommand::ListAgents, &VoiceContext::default()),
        Some("No agents running.".to_string())
    );
    let one = VoiceContext {
        known_agents: vec![agent("a", "build_fix", None)],
        ..VoiceContext::default()
    };
    assert_eq!(
        spoken_response(&VoiceCommand::ListAgents, &one),
        Some("One agent running: build fix.".to_string())
    );
}

#[test]
fn long_lists_are_summarized() {
    let context = VoiceContext {
        known_agents: (1..=12)
            .map(|index| {
                agent(
                    &index.to_string(),
                    &format!("worker-{index}"),
                    Some("desktop"),
                )
            })
            .collect(),
        ..VoiceContext::default()
    };
    let speech = spoken_response(&VoiceCommand::ListAgents, &context).unwrap_or_default();
    assert_eq!(
        speech,
        "12 agents running: worker 1 on desktop, worker 2 on desktop, worker 3 on desktop, and nine more."
    );
    assert!(word_count(&speech) <= MAX_SPOKEN_WORDS);
}

#[test]
fn list_machines_reads_spoken_names() {
    assert_eq!(
        respond("list my machines"),
        Some("Four machines: desktop, laptop, Mac Studio, and one more.".to_string())
    );
    assert_eq!(
        spoken_response(&VoiceCommand::ListMachines, &VoiceContext::default()),
        Some("No machines found.".to_string())
    );
}

#[test]
fn command_acknowledgements() {
    assert_eq!(
        respond("what's the desktop agent doing"),
        Some("Checking build on desktop.".to_string())
    );
    assert_eq!(
        respond("tell build to run the tests"),
        Some("Tell build on desktop: run the tests. Say confirm.".to_string())
    );
    assert_eq!(
        respond("stop the laptop agent"),
        Some("Stop tests on laptop? Say confirm.".to_string())
    );
    assert_eq!(
        respond("switch to claude"),
        Some("Switching to opus.".to_string())
    );
    assert_eq!(
        respond("use mini"),
        Some("Switching to gpt 5.4 mini.".to_string())
    );
    assert_eq!(
        respond("connect to my work box"),
        Some("Connecting to work box.".to_string())
    );
    assert_eq!(respond("tell me how this function works"), None);
}

#[test]
fn ambiguity_and_unknown_targets() {
    let twins = twin_builds();
    assert_eq!(
        spoken_response(&parse_voice_command("stop build", &twins), &twins),
        Some("Which agent: build on desktop or build on laptop?".to_string())
    );
    assert_eq!(
        respond("stop the deploy agent"),
        Some("I don't see an agent called deploy.".to_string())
    );
    assert_eq!(
        respond("switch the model to llama"),
        Some("I don't see a model called llama.".to_string())
    );
    let many = VoiceCommand::Ambiguous {
        action: VoiceAction::StopAgent,
        heard: "worker".to_string(),
        candidates: (1..=5)
            .map(|index| Candidate {
                id: index.to_string(),
                label: format!("worker {index}"),
                machine: None,
            })
            .collect(),
    };
    assert_eq!(
        spoken_response(&many, &VoiceContext::default()),
        Some("Worker matches five agents. Which one?".to_string())
    );
}

#[test]
fn long_steering_messages_are_cut() {
    let speech = respond(
        "tell build to rerun every integration test with verbose logging enabled and then summarize all failures by crate for me",
    )
    .unwrap_or_default();
    assert_eq!(
        speech,
        "Tell build on desktop: rerun every integration test with verbose logging enabled and then… Say confirm."
    );
    assert!(word_count(&speech) <= MAX_SPOKEN_WORDS);
}

#[test]
fn completion_speech() {
    let context = context();
    let completed = |text: &str| completed_speech(&parse_voice_command(text, &context), &context);
    assert_eq!(
        completed("tell build to rebase"),
        Some("Sent to build on desktop.".to_string())
    );
    assert_eq!(
        completed("stop tests"),
        Some("Stopped tests on laptop.".to_string())
    );
    assert_eq!(
        completed("use sonnet"),
        Some("Now using sonnet.".to_string())
    );
    assert_eq!(
        completed("attach to my mac studio"),
        Some("Connected to Mac Studio.".to_string())
    );
    assert_eq!(completed("list agents"), None);
}

#[test]
fn status_answers() {
    let build = agent("t", "build", Some("desktop"));
    assert_eq!(
        agent_status_speech(&build, AgentActivity::Idle, None),
        "Build on desktop is idle."
    );
    assert_eq!(
        agent_status_speech(
            &build,
            AgentActivity::Working,
            Some("\n  Running cargo test -p codex-core\nmore detail")
        ),
        "Build on desktop is working. Latest: Running cargo test -p codex-core."
    );
    assert_eq!(
        agent_status_speech(&build, AgentActivity::WaitingOnApproval, None),
        "Build on desktop is waiting for your approval."
    );
    let long = agent_status_speech(
        &agent(
            "t",
            "a-very-long-agent-name-that-goes-on",
            Some("the-studio-machine"),
        ),
        AgentActivity::Error,
        Some("one two three four five six seven eight nine ten eleven twelve thirteen fourteen"),
    );
    assert!(word_count(&long) <= MAX_SPOKEN_WORDS, "{long}");
}

#[test]
fn every_response_fits_the_word_cap() {
    let context = context();
    for text in [
        "what's running",
        "list my machines",
        "what's build doing",
        "tell docs writer to write a very long and detailed changelog entry covering every single change since the last release",
        "stop the agent on my mac studio",
        "switch to gpt five",
        "connect to my mac studio",
        "stop the deploy agent",
    ] {
        let command = parse_voice_command(text, &context);
        let speech = spoken_response(&command, &context).unwrap_or_default();
        assert!(!speech.is_empty(), "{text:?}");
        assert!(
            word_count(&speech) <= MAX_SPOKEN_WORDS,
            "{text:?}: {speech}"
        );
    }
}

#[test]
fn limit_words_marks_cuts() {
    assert_eq!(limit_words("a b c", 3), "a b c");
    assert_eq!(limit_words("a b c d", 3), "a b c…");
}
