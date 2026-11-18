//! Which agent runs this TUI process's sessions: Codex (the default) or Claude
//! Code (`codex --agent claude-code`). v1 selects one backend per process.

use crate::legacy_core::config::Config;
use codex_app_server_client::AppServerClient;
use codex_claude_agent_backend::ClaudeCodeOptions;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeCodeSelection {
    pub(crate) claude_bin: PathBuf,
}

static CLAUDE_CODE: OnceLock<ClaudeCodeSelection> = OnceLock::new();

/// Selects Claude Code for every session this process starts.
pub(crate) fn select_claude_code(claude_bin: PathBuf) {
    let _ = CLAUDE_CODE.set(ClaudeCodeSelection { claude_bin });
}

pub(crate) fn claude_code() -> Option<&'static ClaudeCodeSelection> {
    CLAUDE_CODE.get()
}

pub(crate) fn is_claude_code() -> bool {
    claude_code().is_some()
}

/// The product name in the session header, the status card and spoken prompts.
pub(crate) fn agent_title(claude_code: bool) -> &'static str {
    if claude_code {
        codex_claude_agent_backend::BACKEND_NAME
    } else {
        "OpenAI Codex"
    }
}

/// The subject of spoken approval prompts ("Claude Code wants to run ...").
pub(crate) fn spoken_agent_name(claude_code: bool) -> &'static str {
    if claude_code {
        codex_claude_agent_backend::BACKEND_NAME
    } else {
        "Codex"
    }
}

/// The options for the Claude Code backend. A configured `model` (from `-m`
/// or config.toml) is passed to `claude` only when it names a Claude model.
pub(crate) fn claude_code_options(
    selection: &ClaudeCodeSelection,
    config: &Config,
) -> ClaudeCodeOptions {
    let mut options = ClaudeCodeOptions::new(config.cwd.as_path().to_path_buf());
    options.claude_bin = selection.claude_bin.clone();
    options.model = config
        .model
        .clone()
        .filter(|model| codex_claude_agent_backend::is_claude_model_id(model));
    if let Some(model) = config.model.as_deref()
        && options.model.is_none()
    {
        tracing::warn!(model, "ignoring a model that is not a Claude model");
    }
    options
}

/// Starts the Claude Code backend for this process's configuration.
pub(crate) fn start_claude_code(
    selection: &ClaudeCodeSelection,
    config: &Config,
) -> AppServerClient {
    codex_claude_agent_backend::start(claude_code_options(selection, config))
}

/// Spoken names for Claude models: each family name (opus, sonnet, haiku,
/// fable) for the first listed model of that family.
pub(crate) fn claude_family_aliases<'a>(
    slugs: impl IntoIterator<Item = &'a str>,
) -> Vec<Option<&'static str>> {
    codex_claude_agent_backend::family_aliases(slugs)
}
