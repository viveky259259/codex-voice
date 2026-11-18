//! `can_use_tool` control requests as app-server approval requests, and the
//! answers back as the SDK's `PermissionResult` (T3 Code's
//! `permissionResultFromDecision`).

use crate::mapping::ToolKind;
use crate::mapping::result_text;
use crate::mapping::tool_kind;
use crate::wire;
use codex_app_server_protocol::AdditionalFileSystemPermissions;
use codex_app_server_protocol::AdditionalNetworkPermissions;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::FileChangeApprovalDecision;
use codex_app_server_protocol::FileChangeRequestApprovalParams;
use codex_app_server_protocol::FileChangeRequestApprovalResponse;
use codex_app_server_protocol::PermissionGrantScope;
use codex_app_server_protocol::PermissionsRequestApprovalParams;
use codex_app_server_protocol::PermissionsRequestApprovalResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::RequestPermissionProfile;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ToolRequestUserInputOption;
use codex_app_server_protocol::ToolRequestUserInputParams;
use codex_app_server_protocol::ToolRequestUserInputQuestion;
use codex_app_server_protocol::ToolRequestUserInputResponse;
use codex_utils_path_uri::LegacyAppPathString;
use serde_json::Value;
use serde_json::json;
use std::path::Path;

pub const DECLINED: &str = "User declined tool execution.";
pub const CANCELLED: &str = "User cancelled tool execution.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalKind {
    Command,
    FileChange,
    Permissions,
    UserInput,
}

/// A `can_use_tool` request waiting for the TUI's answer.
#[derive(Clone, Debug)]
pub struct PendingApproval {
    pub control_request_id: String,
    pub kind: ApprovalKind,
    pub tool_name: String,
    pub tool_use_id: String,
    pub input: Value,
    pub suggestions: Option<Value>,
    pub thread_id: String,
    pub turn_id: String,
}

/// What the CLI is told.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Allow {
        updated_input: Value,
        updated_permissions: Option<Vec<Value>>,
    },
    Deny {
        message: String,
        interrupt: bool,
    },
}

impl Decision {
    pub fn is_deny(&self) -> bool {
        matches!(self, Self::Deny { .. })
    }

    /// The `response` of the `control_response` for this request.
    pub fn permission_result(&self, tool_use_id: &str) -> Value {
        match self {
            Self::Allow {
                updated_input,
                updated_permissions,
            } => wire::permission_allow(
                tool_use_id,
                updated_input.clone(),
                updated_permissions.clone(),
            ),
            Self::Deny { message, interrupt } => {
                wire::permission_deny(tool_use_id, message, *interrupt)
            }
        }
    }
}

/// The approval request for a `can_use_tool` control request (`request` is its
/// inner `request` object).
pub fn approval_request(
    request_id: RequestId,
    control_request_id: &str,
    request: &Value,
    thread_id: &str,
    turn_id: &str,
    cwd: &Path,
    now_ms: i64,
) -> (ServerRequest, PendingApproval) {
    let text = |key: &str| {
        request
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let tool_name = text("tool_name").unwrap_or_default();
    let tool_use_id = text("tool_use_id").unwrap_or_else(|| control_request_id.to_string());
    let input = request.get("input").cloned().unwrap_or(json!({}));
    let reason = text("description")
        .or_else(|| text("decision_reason"))
        .or_else(|| text("title"));
    let item_id = tool_use_id.clone();
    let (kind, server_request) = if tool_name == "AskUserQuestion" {
        (
            ApprovalKind::UserInput,
            ServerRequest::ToolRequestUserInput {
                request_id,
                params: ToolRequestUserInputParams {
                    thread_id: thread_id.to_string(),
                    turn_id: turn_id.to_string(),
                    item_id,
                    questions: user_input_questions(&input),
                    is_blocking: true,
                    auto_resolution_ms: None,
                },
            },
        )
    } else {
        match tool_kind(&tool_name) {
            ToolKind::Command => (
                ApprovalKind::Command,
                ServerRequest::CommandExecutionRequestApproval {
                    request_id,
                    params: CommandExecutionRequestApprovalParams {
                        kind: Default::default(),
                        thread_id: thread_id.to_string(),
                        turn_id: turn_id.to_string(),
                        item_id,
                        started_at_ms: now_ms,
                        approval_id: None,
                        environment_id: None,
                        reason: input
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .or(reason),
                        network_approval_context: None,
                        command: input
                            .get("command")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        cwd: Some(LegacyAppPathString::from_path(cwd)),
                        command_actions: None,
                        additional_permissions: None,
                        proposed_execpolicy_amendment: None,
                        proposed_network_policy_amendments: None,
                        available_decisions: Some(vec![
                            CommandExecutionApprovalDecision::Accept,
                            CommandExecutionApprovalDecision::AcceptForSession,
                            CommandExecutionApprovalDecision::Decline,
                            CommandExecutionApprovalDecision::Cancel,
                        ]),
                    },
                },
            ),
            ToolKind::FileChange => (
                ApprovalKind::FileChange,
                ServerRequest::FileChangeRequestApproval {
                    request_id,
                    params: FileChangeRequestApprovalParams {
                        thread_id: thread_id.to_string(),
                        turn_id: turn_id.to_string(),
                        item_id,
                        started_at_ms: now_ms,
                        reason,
                        grant_root: None,
                    },
                },
            ),
            ToolKind::Mcp { .. } | ToolKind::Dynamic => (
                ApprovalKind::Permissions,
                ServerRequest::PermissionsRequestApproval {
                    request_id,
                    params: PermissionsRequestApprovalParams {
                        thread_id: thread_id.to_string(),
                        turn_id: turn_id.to_string(),
                        item_id,
                        environment_id: None,
                        started_at_ms: now_ms,
                        cwd: LegacyAppPathString::from_path(cwd),
                        reason: Some(permissions_reason(
                            text("display_name").as_deref().unwrap_or(&tool_name),
                            &input,
                            reason.as_deref(),
                        )),
                        permissions: requested_permissions(
                            &tool_name,
                            &input,
                            text("blocked_path").as_deref(),
                            cwd,
                        ),
                    },
                },
            ),
        }
    };
    let pending = PendingApproval {
        control_request_id: control_request_id.to_string(),
        kind,
        tool_name,
        tool_use_id,
        input,
        suggestions: request.get("permission_suggestions").cloned(),
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
    };
    (server_request, pending)
}

/// The permission overlay grants or withholds a profile, so a tool asks for a
/// non-empty one: the network for web tools, else the path it touches.
fn requested_permissions(
    tool_name: &str,
    input: &Value,
    blocked_path: Option<&str>,
    cwd: &Path,
) -> RequestPermissionProfile {
    if matches!(tool_name, "WebFetch" | "WebSearch") {
        return RequestPermissionProfile {
            network: Some(AdditionalNetworkPermissions {
                enabled: Some(true),
            }),
            file_system: None,
        };
    }
    let path = blocked_path
        .map(str::to_string)
        .or_else(|| {
            ["file_path", "path", "notebook_path"]
                .into_iter()
                .find_map(|key| input.get(key).and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| cwd.to_string_lossy().into_owned());
    let paths = Some(vec![LegacyAppPathString::from_string(path)]);
    let reads = matches!(tool_name, "Read" | "Glob" | "Grep" | "LS");
    RequestPermissionProfile {
        network: None,
        file_system: Some(AdditionalFileSystemPermissions {
            read: if reads { paths.clone() } else { None },
            write: if reads { None } else { paths },
            glob_scan_max_depth: None,
            entries: None,
        }),
    }
}

fn permissions_reason(tool: &str, input: &Value, reason: Option<&str>) -> String {
    let detail = reason.map(str::to_string).unwrap_or_else(|| {
        let mut text = result_text(input);
        if text.chars().count() > 160 {
            text = text.chars().take(157).collect::<String>() + "...";
        }
        text
    });
    if detail.is_empty() {
        format!("Claude Code wants to use {tool}.")
    } else {
        format!("Claude Code wants to use {tool}: {detail}")
    }
}

/// AskUserQuestion's questions, keyed by their text as the SDK keys answers.
fn user_input_questions(input: &Value) -> Vec<ToolRequestUserInputQuestion> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, question)| {
            let text = question.get("question").and_then(Value::as_str)?.trim();
            if text.is_empty() {
                return None;
            }
            let header = question
                .get("header")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|header| !header.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Question {}", index + 1));
            let options: Vec<ToolRequestUserInputOption> = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    let label = option.get("label").and_then(Value::as_str)?.trim();
                    (!label.is_empty()).then(|| ToolRequestUserInputOption {
                        label: label.to_string(),
                        description: option
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|description| !description.is_empty())
                            .unwrap_or(label)
                            .to_string(),
                    })
                })
                .collect();
            Some(ToolRequestUserInputQuestion {
                id: text.to_string(),
                header,
                question: text.to_string(),
                is_other: true,
                is_secret: false,
                options: (!options.is_empty()).then_some(options),
            })
        })
        .collect()
}

/// The TUI's answer (`result` of the server request) as a decision.
pub fn decision_from_response(pending: &PendingApproval, result: &Value) -> Decision {
    let accept = |for_session: bool| Decision::Allow {
        updated_input: pending.input.clone(),
        updated_permissions: for_session.then(|| {
            wire::session_permission_updates(&pending.tool_name, pending.suggestions.as_ref())
        }),
    };
    let decline = || Decision::Deny {
        message: DECLINED.to_string(),
        interrupt: false,
    };
    let cancel = || Decision::Deny {
        message: CANCELLED.to_string(),
        interrupt: true,
    };
    let unreadable = || Decision::Deny {
        message: "The approval answer could not be read.".to_string(),
        interrupt: false,
    };
    match pending.kind {
        ApprovalKind::Command => {
            match serde_json::from_value::<CommandExecutionRequestApprovalResponse>(result.clone())
            {
                Ok(response) => match response.decision {
                    CommandExecutionApprovalDecision::Accept
                    | CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment { .. }
                    | CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment { .. } => {
                        accept(false)
                    }
                    CommandExecutionApprovalDecision::AcceptForSession => accept(true),
                    CommandExecutionApprovalDecision::Decline => decline(),
                    CommandExecutionApprovalDecision::Cancel => cancel(),
                },
                Err(_) => unreadable(),
            }
        }
        ApprovalKind::FileChange => {
            match serde_json::from_value::<FileChangeRequestApprovalResponse>(result.clone()) {
                Ok(response) => match response.decision {
                    FileChangeApprovalDecision::Accept => accept(false),
                    FileChangeApprovalDecision::AcceptForSession => accept(true),
                    FileChangeApprovalDecision::Decline => decline(),
                    FileChangeApprovalDecision::Cancel => cancel(),
                },
                Err(_) => unreadable(),
            }
        }
        ApprovalKind::Permissions => {
            match serde_json::from_value::<PermissionsRequestApprovalResponse>(result.clone()) {
                Ok(response) => {
                    let granted = response.permissions.network.is_some()
                        || response.permissions.file_system.is_some();
                    if !granted {
                        decline()
                    } else {
                        accept(response.scope == PermissionGrantScope::Session)
                    }
                }
                Err(_) => unreadable(),
            }
        }
        ApprovalKind::UserInput => {
            match serde_json::from_value::<ToolRequestUserInputResponse>(result.clone()) {
                Ok(response) => {
                    let answers: serde_json::Map<String, Value> = response
                        .answers
                        .into_iter()
                        .map(|(question, answer)| {
                            (question, Value::String(answer.answers.join(", ")))
                        })
                        .collect();
                    if answers.is_empty() {
                        return Decision::Deny {
                            message: "The user did not answer.".to_string(),
                            interrupt: false,
                        };
                    }
                    Decision::Allow {
                        updated_input: json!({
                            "questions": pending.input.get("questions").cloned().unwrap_or(json!([])),
                            "answers": answers,
                        }),
                        updated_permissions: None,
                    }
                }
                Err(_) => unreadable(),
            }
        }
    }
}

#[cfg(test)]
#[path = "approvals_tests.rs"]
mod tests;
