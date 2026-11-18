//! The `claude` CLI's stream-json wire format, as the Claude Agent SDK
//! (`@anthropic-ai/claude-agent-sdk` 0.3.327) speaks it. See README.md.
//!
//! Every message is one JSON object per line, on the CLI's stdin and stdout.

use serde_json::Value;
use serde_json::json;
use std::ffi::OsString;

/// How a process is attached to a Claude Code session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// A new session with an id we choose (`--session-id=<uuid>`).
    New { session_id: String },
    /// Continue a saved session (`--resume=<uuid>`).
    Resume { session_id: String },
}

impl Launch {
    pub fn session_id(&self) -> &str {
        match self {
            Self::New { session_id } | Self::Resume { session_id } => session_id,
        }
    }
}

/// The argv the SDK builds for a streaming `query()` with `canUseTool`,
/// `includePartialMessages`, adaptive summarized thinking and the default
/// permission mode (the options T3 Code passes), in the SDK's order.
/// `effort` is the SDK's `effort` option (`--effort=<level>`).
pub fn claude_args(model: Option<&str>, effort: Option<&str>, launch: &Launch) -> Vec<OsString> {
    let mut args: Vec<String> = vec![
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--thinking=adaptive".into(),
        "--thinking-display=summarized".into(),
    ];
    if let Some(effort) = effort {
        args.push(format!("--effort={effort}"));
    }
    if let Some(model) = model {
        args.push(format!("--model={model}"));
    }
    args.push("--permission-prompt-tool=stdio".into());
    if let Launch::Resume { session_id } = launch {
        args.push(format!("--resume={session_id}"));
    }
    args.push("--permission-mode=default".into());
    args.push("--include-partial-messages".into());
    if let Launch::New { session_id } = launch {
        args.push(format!("--session-id={session_id}"));
    }
    // T3 Code turns thinking summaries on in the SDK's `settings` option, which
    // the SDK passes last, as `--settings <json>`.
    args.push("--settings".into());
    args.push(r#"{"showThinkingSummaries":true}"#.into());
    args.into_iter().map(OsString::from).collect()
}

/// `{"type":"control_request","request_id":ID,"request":{"subtype":...}}`
pub fn control_request(request_id: &str, request: Value) -> Value {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": request,
    })
}

/// The first control request. We register no hooks, SDK MCP servers or agents.
pub fn initialize_request() -> Value {
    json!({ "subtype": "initialize" })
}

pub fn interrupt_request() -> Value {
    json!({ "subtype": "interrupt" })
}

/// `model: null` returns to Claude Code's default model.
pub fn set_model_request(model: Option<&str>) -> Value {
    json!({ "subtype": "set_model", "model": model })
}

/// The SDK's `applyFlagSettings({effortLevel})`: the effort for the rest of
/// the session. `null` returns to the model's default effort.
pub fn set_effort_request(effort: Option<&str>) -> Value {
    json!({ "subtype": "apply_flag_settings", "settings": { "effortLevel": effort } })
}

/// The answer to a control request the CLI sent us (for example `can_use_tool`).
pub fn control_success(request_id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        },
    })
}

/// Refuses a control request this client does not handle.
pub fn control_error(request_id: &str, error: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": error,
        },
    })
}

/// A user turn, exactly as the SDK writes a string prompt.
pub fn user_message(text: &str) -> Value {
    json!({
        "type": "user",
        "session_id": "",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": text }],
        },
        "parent_tool_use_id": null,
    })
}

/// `PermissionResult` `allow`. The SDK echoes the tool's input as `updatedInput`.
pub fn permission_allow(
    tool_use_id: &str,
    updated_input: Value,
    updated_permissions: Option<Vec<Value>>,
) -> Value {
    let mut result = json!({
        "behavior": "allow",
        "updatedInput": updated_input,
        "toolUseID": tool_use_id,
    });
    if let Some(updates) = updated_permissions {
        result["updatedPermissions"] = Value::Array(updates);
    }
    result
}

/// `PermissionResult` `deny`; `interrupt` also stops the turn.
pub fn permission_deny(tool_use_id: &str, message: &str, interrupt: bool) -> Value {
    let mut result = json!({
        "behavior": "deny",
        "message": message,
        "toolUseID": tool_use_id,
    });
    if interrupt {
        result["interrupt"] = Value::Bool(true);
    }
    result
}

/// "Allow for this session": the CLI's own suggestions moved to the session,
/// or else an allow rule for the whole tool (T3 Code's `toSessionPermissionUpdates`).
pub fn session_permission_updates(tool_name: &str, suggestions: Option<&Value>) -> Vec<Value> {
    let updates: Vec<Value> = suggestions
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|suggestion| suggestion.is_object())
        .map(|suggestion| {
            let mut suggestion = suggestion.clone();
            suggestion["destination"] = json!("session");
            suggestion
        })
        .collect();
    if !updates.is_empty() {
        return updates;
    }
    vec![json!({
        "type": "addRules",
        "rules": [{ "toolName": tool_name }],
        "behavior": "allow",
        "destination": "session",
    })]
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
