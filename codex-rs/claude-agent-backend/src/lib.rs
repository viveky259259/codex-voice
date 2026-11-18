//! Claude Code agent sessions behind the app-server protocol.
//!
//! [`start`] returns an [`AppServerClient`] that answers the requests a TUI
//! sends (thread start/resume/read/list, turn start/interrupt, model list,
//! settings updates) by driving the `claude` CLI over its stream-json and SDK
//! control protocol, and emits the notifications and approval requests a Codex
//! app server would. Authentication is the CLI's own login. See README.md for
//! the wire format and what is mapped.

mod approvals;
mod diff;
mod mapping;
pub mod models;
pub mod wire;
mod worker;

use codex_app_server_client::AgentBackendClient;
use codex_app_server_client::AppServerClient;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// Shown in the TUI header, status card and error messages.
pub const BACKEND_NAME: &str = "Claude Code";
/// `modelProvider` on threads served by this backend.
pub const MODEL_PROVIDER: &str = "claude-code";
/// The value of `codex --agent` that selects this backend.
pub const AGENT_NAME: &str = "claude-code";

#[derive(Clone, Debug)]
pub struct ClaudeCodeOptions {
    /// The `claude` executable (default `claude`, found on `PATH`).
    pub claude_bin: PathBuf,
    /// Passed as `--model`. `None` uses Claude Code's default.
    pub model: Option<String>,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// Extra environment for the CLI.
    pub env: Vec<(OsString, OsString)>,
    /// How long `model/list` waits for the CLI's `initialize` response.
    pub init_timeout: Duration,
}

impl ClaudeCodeOptions {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            claude_bin: PathBuf::from("claude"),
            model: None,
            cwd,
            env: Vec::new(),
            init_timeout: Duration::from_secs(15),
        }
    }
}

/// Starts the backend on the current tokio runtime. No process is spawned until
/// the first request that needs one.
pub fn start(options: ClaudeCodeOptions) -> AppServerClient {
    let (client, commands, events) = AgentBackendClient::channel(BACKEND_NAME, 32);
    tokio::spawn(worker::run(options, commands, events));
    AppServerClient::AgentBackend(client)
}

pub use models::family_aliases;
pub use models::is_claude_model_id;

/// A fake `claude` for tests in this and other crates: tests/fake_claude.py.
#[doc(hidden)]
pub mod test_support {
    use std::path::Path;
    use std::path::PathBuf;

    pub const FAKE_CLAUDE_PY: &str = include_str!("../tests/fake_claude.py");

    /// Writes the fake as `dir/claude`, run by `python3 -I`. `None` when
    /// python3 is not on PATH.
    pub fn write_fake_claude(dir: &Path) -> Option<PathBuf> {
        let python = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("python3"))
                .find(|path| path.is_file())
        })?;
        let script = dir.join("claude");
        std::fs::write(
            &script,
            format!("#!{} -I\n{FAKE_CLAUDE_PY}", python.display()),
        )
        .ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).ok()?;
        }
        Some(script)
    }
}
