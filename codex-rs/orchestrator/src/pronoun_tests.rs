use super::resolve_pronoun;
use codex_voice_commands::AgentRef;
use codex_voice_commands::VoiceCommand;
use pretty_assertions::assert_eq;

fn last() -> AgentRef {
    AgentRef {
        id: "t-deploy".to_string(),
        name: "deploy-to-prod".to_string(),
        machine: Some("desktop".to_string()),
    }
}

#[test]
fn pronouns_become_the_last_agent() {
    let status = VoiceCommand::AgentStatus { agent: last() };
    let stop = VoiceCommand::StopAgent {
        agent: last(),
        needs_confirmation: true,
    };
    let steer = |message: &str| VoiceCommand::SteerAgent {
        agent: last(),
        message: message.to_string(),
        needs_confirmation: true,
    };
    let cases = [
        ("what's it doing", Some(status.clone())),
        ("What is that agent working on?", Some(status.clone())),
        ("check on it", Some(status)),
        ("stop it", Some(stop.clone())),
        ("interrupt the same agent", Some(stop)),
        // The name contains "to"; the message keeps its casing and its own "it".
        (
            "tell it to retry it with RUST_LOG=debug",
            Some(steer("retry it with RUST_LOG=debug")),
        ),
        (
            "ask that one to run the tests.",
            Some(steer("run the tests")),
        ),
        ("tell me how it works", None),
        ("is it raining", None),
        ("stop", None),
        ("what does this function do", None),
    ];
    for (transcript, expected) in cases {
        assert_eq!(
            resolve_pronoun(transcript, &last()),
            expected,
            "{transcript}"
        );
    }
}
