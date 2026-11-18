use super::*;
use crate::test_fixtures::agent;
use crate::test_fixtures::context;
use crate::test_fixtures::twin_builds;
use pretty_assertions::assert_eq;

fn parse(text: &str) -> VoiceCommand {
    parse_voice_command(text, &context())
}

fn build() -> AgentRef {
    agent("t-build", "build", Some("desktop"))
}

fn tests_agent() -> AgentRef {
    agent("t-tests", "tests", Some("laptop"))
}

fn docs() -> AgentRef {
    agent("t-docs", "docs-writer", Some("MacStudio"))
}

fn status(agent: AgentRef) -> VoiceCommand {
    VoiceCommand::AgentStatus { agent }
}

fn steer(agent: AgentRef, message: &str) -> VoiceCommand {
    VoiceCommand::SteerAgent {
        agent,
        message: message.to_string(),
        needs_confirmation: true,
    }
}

fn stop(agent: AgentRef) -> VoiceCommand {
    VoiceCommand::StopAgent {
        agent,
        needs_confirmation: true,
    }
}

fn switch(slug: &str) -> VoiceCommand {
    VoiceCommand::SwitchModel {
        slug: slug.to_string(),
    }
}

fn attach(machine: &str) -> VoiceCommand {
    VoiceCommand::AttachMachine {
        machine: machine.to_string(),
    }
}

fn candidate(id: &str, label: &str) -> Candidate {
    Candidate {
        id: id.to_string(),
        label: label.to_string(),
        machine: None,
    }
}

fn agent_candidate(id: &str, label: &str, machine: &str) -> Candidate {
    Candidate {
        machine: Some(machine.to_string()),
        ..candidate(id, label)
    }
}

#[test]
fn list_agents_phrases() {
    for text in [
        "What's running?",
        "whats running",
        "what is running right now",
        "List agents.",
        "list my agents",
        "Show my agents",
        "show me all my agents please",
        "hey codex, what agents are running",
        "Okay, can you list the agents",
    ] {
        assert_eq!(parse(text), VoiceCommand::ListAgents, "{text:?}");
    }
}

#[test]
fn list_machines_phrases() {
    for text in [
        "List my machines.",
        "show my machines",
        "what machines do I have",
        "which machines are online?",
    ] {
        assert_eq!(parse(text), VoiceCommand::ListMachines, "{text:?}");
    }
}

#[test]
fn agent_status_by_name_and_machine() {
    let cases = [
        ("What's build doing?", build()),
        ("what is the build agent doing", build()),
        ("What is the desktop agent doing?", build()),
        ("what's my laptop agent up to", tests_agent()),
        ("status of tests", tests_agent()),
        ("what's the status of the docs writer", docs()),
        ("How's the agent on my mac studio doing?", docs()),
        ("check on docs-writer", docs()),
        ("what is tests working on right now", tests_agent()),
        ("um, what's desktop doing", build()),
    ];
    for (text, agent) in cases {
        assert_eq!(parse(text), status(agent), "{text:?}");
    }
}

#[test]
fn steer_agent_keeps_the_spoken_message() {
    assert_eq!(
        parse("Tell build to run the tests."),
        steer(build(), "run the tests")
    );
    assert_eq!(
        parse("ask the laptop agent to fix parser.rs and rerun CI"),
        steer(tests_agent(), "fix parser.rs and rerun CI")
    );
    assert_eq!(
        parse("Hey Codex, can you tell my docs writer to add a changelog entry?"),
        steer(docs(), "add a changelog entry?")
    );
    assert_eq!(
        parse("tell the agent on desktop to stop using unwrap"),
        steer(build(), "stop using unwrap")
    );
}

#[test]
fn steer_target_may_contain_to() {
    let context = VoiceContext {
        known_agents: vec![agent("t", "path to prod", None)],
        ..VoiceContext::default()
    };
    assert_eq!(
        parse_voice_command("tell path to prod to deploy", &context),
        steer(agent("t", "path to prod", None), "deploy")
    );
}

#[test]
fn stop_agent_by_name_or_machine() {
    let cases = [
        ("Stop build", build()),
        ("stop the build agent now", build()),
        ("interrupt the laptop agent", tests_agent()),
        ("please stop the agent on my mac studio", docs()),
        ("Interrupt docs writer.", docs()),
    ];
    for (text, agent) in cases {
        assert_eq!(parse(text), stop(agent), "{text:?}");
    }
}

#[test]
fn destructive_commands_need_confirmation() {
    assert!(parse("stop build").needs_confirmation());
    assert!(parse("tell build to run the tests").needs_confirmation());
    assert!(!parse("what's build doing").needs_confirmation());
    assert!(!parse("switch to opus").needs_confirmation());
    assert!(!parse("connect to desktop").needs_confirmation());
}

#[test]
fn switch_model_through_aliases() {
    let cases = [
        ("switch to claude", "claude-opus-5-5"),
        ("Use opus.", "claude-opus-5-5"),
        ("use the opus model", "claude-opus-5-5"),
        ("switch to GPT five", "gpt-5.5"),
        ("switch to gpt 5.5", "gpt-5.5"),
        ("switch the model to sonnet", "claude-sonnet-5"),
        ("change model to claude sonnet 5", "claude-sonnet-5"),
        ("use mini", "gpt-5.4-mini"),
    ];
    for (text, slug) in cases {
        assert_eq!(parse(text), switch(slug), "{text:?}");
    }
}

#[test]
fn attach_machine_with_spoken_forms() {
    let cases = [
        ("Connect to my Mac Studio.", "MacStudio"),
        ("attach to desktop", "desktop"),
        ("connect me to the laptop", "laptop"),
        ("ssh into work box", "work_box"),
        ("connect to my macstudio machine", "MacStudio"),
    ];
    for (text, machine) in cases {
        assert_eq!(parse(text), attach(machine), "{text:?}");
    }
}

#[test]
fn ambiguous_targets_are_not_guessed() {
    let context = twin_builds();
    let builds = vec![
        agent_candidate("t-build-desktop", "build on desktop", "desktop"),
        agent_candidate("t-build-laptop", "build on laptop", "laptop"),
    ];
    assert_eq!(
        parse_voice_command("stop build", &context),
        VoiceCommand::Ambiguous {
            action: VoiceAction::StopAgent,
            heard: "build".to_string(),
            candidates: builds.clone(),
        }
    );
    assert_eq!(
        parse_voice_command("tell build to rebase", &context),
        VoiceCommand::Ambiguous {
            action: VoiceAction::SteerAgent {
                message: "rebase".to_string()
            },
            heard: "build".to_string(),
            candidates: builds,
        }
    );
    assert_eq!(
        parse_voice_command("switch to claude", &context),
        VoiceCommand::Ambiguous {
            action: VoiceAction::SwitchModel,
            heard: "claude".to_string(),
            candidates: vec![
                candidate("claude-opus-5-5", "claude opus 5 5"),
                candidate("claude-sonnet-5", "claude sonnet 5"),
            ],
        }
    );
    assert_eq!(
        parse_voice_command("what's build on laptop doing", &context),
        status(agent("t-build-laptop", "build", Some("laptop")))
    );
}

#[test]
fn explicitly_named_but_unknown_targets_are_reported() {
    assert_eq!(
        parse("stop the deploy agent"),
        VoiceCommand::NotFound {
            action: VoiceAction::StopAgent,
            heard: "deploy".to_string(),
        }
    );
    assert_eq!(
        parse("what's the staging agent doing"),
        VoiceCommand::NotFound {
            action: VoiceAction::AgentStatus,
            heard: "staging".to_string(),
        }
    );
    assert_eq!(
        parse("tell the deploy agent to roll back"),
        VoiceCommand::NotFound {
            action: VoiceAction::SteerAgent {
                message: "roll back".to_string()
            },
            heard: "deploy".to_string(),
        }
    );
    assert_eq!(
        parse("switch the model to llama"),
        VoiceCommand::NotFound {
            action: VoiceAction::SwitchModel,
            heard: "llama".to_string(),
        }
    );
    assert_eq!(
        parse("connect to my gaming machine"),
        VoiceCommand::NotFound {
            action: VoiceAction::AttachMachine,
            heard: "gaming".to_string(),
        }
    );
}

#[test]
fn ordinary_coding_requests_pass_through() {
    for text in [
        "",
        "   ",
        "stop",
        "Stop.",
        "stop it",
        "stop this",
        "cancel",
        "interrupt",
        "tell me how this function works",
        "tell me how to fix the build",
        "ask the user to confirm before deleting",
        "tell the parser to skip comments",
        "stop using unwrap in parser.rs",
        "stop the server before running migrations",
        "stop the agent from looping forever",
        "switch the order of these args",
        "switch to the main branch",
        "switch to the model view in the app",
        "use tabs instead of spaces",
        "use opus for the audio codec",
        "use five spaces",
        "connect to the database",
        "connect to postgres on port 5432",
        "attach to the running process",
        "what is this function doing",
        "what's the parser doing here",
        "what is going on",
        "what's running on port 3000",
        "list the files in src",
        "show my changes",
        "how is the test suite doing",
        "status of the migration",
        "check on the flaky test",
        "don't stop build",
        "please run the tests on build",
        "build the project",
    ] {
        assert_eq!(parse(text), VoiceCommand::None, "{text:?}");
    }
}

#[test]
fn long_utterances_are_prompts() {
    assert_eq!(
        parse("what is build doing with all of those files in the source directory today"),
        VoiceCommand::None
    );
    // Steering messages may be long.
    assert_eq!(
        parse(
            "tell build to rerun the integration tests with logging enabled and send me the failures"
        ),
        steer(
            build(),
            "rerun the integration tests with logging enabled and send me the failures"
        )
    );
}

#[test]
fn empty_steer_message_is_not_a_command() {
    assert_eq!(parse("tell build to"), VoiceCommand::None);
    assert_eq!(parse("tell build to."), VoiceCommand::None);
    assert_eq!(parse("tell build"), VoiceCommand::None);
}

#[test]
fn bare_agent_resolves_when_only_one_runs() {
    let context = VoiceContext {
        known_agents: vec![build()],
        ..VoiceContext::default()
    };
    assert_eq!(
        parse_voice_command("what is the agent doing", &context),
        status(build())
    );
    assert_eq!(
        parse_voice_command("what is the agent doing", &VoiceContext::default()),
        VoiceCommand::None
    );
}

#[test]
fn empty_context_never_produces_targets() {
    let empty = VoiceContext::default();
    for text in [
        "stop build",
        "tell build to run the tests",
        "switch to claude",
        "connect to desktop",
        "what's build doing",
    ] {
        assert_eq!(
            parse_voice_command(text, &empty),
            VoiceCommand::None,
            "{text:?}"
        );
    }
    assert_eq!(
        parse_voice_command("list agents", &empty),
        VoiceCommand::ListAgents
    );
}

#[test]
fn confirmation_mirrors_the_approval_grammar() {
    for text in [
        "confirm",
        "Confirm.",
        "yes, confirm",
        "ok confirm please",
        "I confirm",
    ] {
        assert!(is_voice_confirmation(text), "{text:?}");
    }
    for text in [
        "",
        "yes",
        "approve",
        "don't confirm",
        "wait confirm",
        "confirm the build is green before you merge",
        "maybe confirm",
    ] {
        assert!(!is_voice_confirmation(text), "{text:?}");
    }
}

#[test]
fn choices_resolve_ambiguity() {
    let context = twin_builds();
    let VoiceCommand::Ambiguous {
        action, candidates, ..
    } = parse_voice_command("stop build", &context)
    else {
        panic!("expected an ambiguous stop");
    };
    let laptop = stop(agent("t-build-laptop", "build", Some("laptop")));
    let desktop = stop(agent("t-build-desktop", "build", Some("desktop")));
    for (answer, expected) in [
        ("the one on laptop", Some(laptop.clone())),
        ("laptop", Some(laptop.clone())),
        ("the second one", Some(laptop)),
        ("first", Some(desktop.clone())),
        ("number one", Some(desktop)),
        ("build", None),
        ("number three", None),
        ("never mind", None),
    ] {
        assert_eq!(
            resolve_choice(&action, &candidates, answer, &context),
            expected,
            "{answer:?}"
        );
    }
}

#[test]
fn choices_keep_the_steering_message() {
    let context = twin_builds();
    let VoiceCommand::Ambiguous {
        action, candidates, ..
    } = parse_voice_command("tell build to rebase on main", &context)
    else {
        panic!("expected an ambiguous steer");
    };
    assert_eq!(
        resolve_choice(&action, &candidates, "desktop", &context),
        Some(steer(
            agent("t-build-desktop", "build", Some("desktop")),
            "rebase on main"
        ))
    );
}

#[test]
fn model_and_machine_choices() {
    let context = twin_builds();
    let VoiceCommand::Ambiguous {
        action, candidates, ..
    } = parse_voice_command("use claude", &context)
    else {
        panic!("expected an ambiguous model");
    };
    assert_eq!(
        resolve_choice(&action, &candidates, "sonnet", &context),
        Some(switch("claude-sonnet-5"))
    );
    assert_eq!(
        resolve_choice(
            &VoiceAction::AttachMachine,
            &[
                candidate("desktop", "desktop"),
                candidate("laptop", "laptop")
            ],
            "the laptop",
            &context,
        ),
        Some(attach("laptop"))
    );
}

#[test]
fn choices_between_machines_with_the_same_thread_id_pick_the_right_machine() {
    // Thread ids are only unique per machine.
    let context = VoiceContext {
        known_agents: vec![
            agent("t-1", "build", Some("desktop")),
            agent("t-1", "build", Some("laptop")),
        ],
        known_machines: vec!["desktop".to_string(), "laptop".to_string()],
        known_models: Vec::new(),
    };
    let VoiceCommand::Ambiguous {
        action, candidates, ..
    } = parse_voice_command("stop build", &context)
    else {
        panic!("expected an ambiguous stop");
    };
    assert_eq!(
        resolve_choice(&action, &candidates, "laptop", &context),
        Some(stop(agent("t-1", "build", Some("laptop"))))
    );
    assert_eq!(
        resolve_choice(&action, &candidates, "first", &context),
        Some(stop(agent("t-1", "build", Some("desktop"))))
    );
}
