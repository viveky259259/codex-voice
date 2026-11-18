//! Shared contexts for unit tests.

use crate::AgentRef;
use crate::ModelRef;
use crate::VoiceContext;

pub(crate) fn agent(id: &str, name: &str, machine: Option<&str>) -> AgentRef {
    AgentRef {
        id: id.to_string(),
        name: name.to_string(),
        machine: machine.map(str::to_string),
    }
}

pub(crate) fn model(slug: &str, aliases: &[&str]) -> ModelRef {
    ModelRef {
        slug: slug.to_string(),
        spoken_aliases: aliases.iter().map(|alias| (*alias).to_string()).collect(),
    }
}

/// build on desktop, tests on laptop, docs-writer on MacStudio.
pub(crate) fn context() -> VoiceContext {
    VoiceContext {
        known_agents: vec![
            agent("t-build", "build", Some("desktop")),
            agent("t-tests", "tests", Some("laptop")),
            agent("t-docs", "docs-writer", Some("MacStudio")),
        ],
        known_machines: vec![
            "desktop".to_string(),
            "laptop".to_string(),
            "MacStudio".to_string(),
            "work_box".to_string(),
        ],
        known_models: vec![
            model("claude-opus-5-5", &["opus", "claude", "claude opus"]),
            model("claude-sonnet-5", &["sonnet"]),
            model("gpt-5.5", &["gpt five", "gpt five point five"]),
            model("gpt-5.4-mini", &[]),
        ],
    }
}

/// Two agents named build on different machines.
pub(crate) fn twin_builds() -> VoiceContext {
    VoiceContext {
        known_agents: vec![
            agent("t-build-desktop", "build", Some("desktop")),
            agent("t-build-laptop", "build", Some("laptop")),
            agent("t-lint", "lint", Some("desktop")),
        ],
        known_machines: vec!["desktop".to_string(), "laptop".to_string()],
        known_models: vec![
            model("claude-opus-5-5", &["claude", "opus"]),
            model("claude-sonnet-5", &["claude", "sonnet"]),
        ],
    }
}
