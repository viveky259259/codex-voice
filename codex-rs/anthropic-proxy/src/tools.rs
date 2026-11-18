//! Maps Responses API tool declarations to Anthropic tools and back.

use std::collections::HashMap;

use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use tracing::debug;
use tracing::warn;

/// Property that carries the raw text of a freeform (`custom`) tool call.
pub(crate) const CUSTOM_TOOL_INPUT_PROPERTY: &str = "input";

/// Anthropic tool names must match `^[a-zA-Z0-9_-]{1,64}$`.
const MAX_TOOL_NAME_LEN: usize = 64;

/// Separator between a namespace and a tool name in a flattened tool name.
const NAMESPACE_SEPARATOR: &str = "__";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolKind {
    /// JSON-argument `function` tool, re-emitted as `function_call`.
    Function,
    /// Freeform `custom` tool, re-emitted as `custom_tool_call`.
    Custom,
}

/// The Codex tool behind an Anthropic tool name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolTarget {
    pub(crate) kind: ToolKind,
    pub(crate) name: String,
    pub(crate) namespace: Option<String>,
}

/// Tools declared on one request, keyed by the Anthropic-facing name so the
/// stream translator can rebuild the Codex call item.
#[derive(Debug, Default, Clone)]
pub(crate) struct ToolRegistry {
    by_anthropic_name: HashMap<String, ToolTarget>,
}

impl ToolRegistry {
    pub(crate) fn lookup(&self, anthropic_name: &str) -> Option<&ToolTarget> {
        self.by_anthropic_name.get(anthropic_name)
    }

    /// Resolves a streamed tool name, falling back to a plain function call
    /// for names that were not declared on the request.
    pub(crate) fn resolve(&self, anthropic_name: &str) -> ToolTarget {
        self.lookup(anthropic_name)
            .cloned()
            .unwrap_or_else(|| ToolTarget {
                kind: ToolKind::Function,
                name: anthropic_name.to_string(),
                namespace: None,
            })
    }

    fn register(&mut self, anthropic_name: String, target: ToolTarget) {
        if let Some(previous) = self.by_anthropic_name.get(&anthropic_name)
            && previous != &target
        {
            warn!(
                tool = %anthropic_name,
                "two tools map to the same Anthropic tool name; the later one wins"
            );
        }
        self.by_anthropic_name.insert(anthropic_name, target);
    }
}

/// Returns the Anthropic tool name for a Codex tool. Deterministic, so tool
/// calls in history map to the same name as the declared tool.
pub(crate) fn anthropic_tool_name(namespace: Option<&str>, name: &str) -> String {
    let raw = match namespace.filter(|namespace| !namespace.is_empty()) {
        Some(namespace) => format!("{namespace}{NAMESPACE_SEPARATOR}{name}"),
        None => name.to_string(),
    };
    let is_valid = !raw.is_empty()
        && raw.len() <= MAX_TOOL_NAME_LEN
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if is_valid {
        return raw;
    }
    // Disambiguate rewritten names with a stable hash of the original name.
    let hash = fnv1a_32(raw.as_bytes());
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_TOOL_NAME_LEN - 9)
        .collect();
    format!("{cleaned}_{hash:08x}")
}

fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Translates the Responses `tools` array into Anthropic tool definitions and
/// records how to map Anthropic tool calls back to Codex items.
pub(crate) fn translate_tools(tools: &[Value], registry: &mut ToolRegistry) -> Vec<Value> {
    let mut out = Vec::new();
    for tool in tools {
        translate_tool(tool, /*namespace*/ None, registry, &mut out);
    }
    out
}

fn translate_tool(
    tool: &Value,
    namespace: Option<&str>,
    registry: &mut ToolRegistry,
    out: &mut Vec<Value>,
) {
    let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or_default();
    match tool_type {
        "function" => {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                debug!("dropping function tool without a name");
                return;
            };
            let anthropic_name = anthropic_tool_name(namespace, name);
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut definition = json!({
                "name": anthropic_name,
                "input_schema": object_schema(tool.get("parameters")),
            });
            if !description.is_empty() {
                definition["description"] = Value::String(description.to_string());
            }
            out.push(definition);
            registry.register(
                anthropic_name,
                ToolTarget {
                    kind: ToolKind::Function,
                    name: name.to_string(),
                    namespace: namespace.map(str::to_string),
                },
            );
        }
        "custom" => {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                debug!("dropping custom tool without a name");
                return;
            };
            let anthropic_name = anthropic_tool_name(namespace, name);
            out.push(json!({
                "name": anthropic_name,
                "description": custom_tool_description(tool),
                "input_schema": {
                    "type": "object",
                    "properties": {
                        CUSTOM_TOOL_INPUT_PROPERTY: {
                            "type": "string",
                            "description": "The complete raw tool input as plain text.",
                        },
                    },
                    "required": [CUSTOM_TOOL_INPUT_PROPERTY],
                },
            }));
            registry.register(
                anthropic_name,
                ToolTarget {
                    kind: ToolKind::Custom,
                    name: name.to_string(),
                    namespace: namespace.map(str::to_string),
                },
            );
        }
        "namespace" => {
            let Some(namespace_name) = tool.get("name").and_then(Value::as_str) else {
                debug!("dropping namespace tool without a name");
                return;
            };
            if namespace.is_some() {
                debug!(namespace = namespace_name, "dropping nested namespace tool");
                return;
            }
            for inner in tool
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                translate_tool(inner, Some(namespace_name), registry, out);
            }
        }
        // Hosted OpenAI tools have no Anthropic client-tool equivalent.
        other => {
            debug!(
                tool_type = other,
                "dropping tool with no Anthropic equivalent"
            );
        }
    }
}

/// Anthropic requires `input_schema` to be a JSON object schema.
fn object_schema(parameters: Option<&Value>) -> Value {
    match parameters {
        Some(Value::Object(schema)) => {
            let mut schema = schema.clone();
            schema
                .entry("type")
                .or_insert_with(|| Value::String("object".to_string()));
            if schema.get("type").and_then(Value::as_str) == Some("object") {
                schema
                    .entry("properties")
                    .or_insert_with(|| Value::Object(Map::new()));
                Value::Object(schema)
            } else {
                // Arguments are forwarded verbatim, so a wrapper property would
                // change what Codex receives; accept any object instead.
                debug!("replacing non-object tool parameters with an open object schema");
                json!({"type": "object", "properties": {}})
            }
        }
        _ => json!({"type": "object", "properties": {}}),
    }
}

fn custom_tool_description(tool: &Value) -> String {
    let mut description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    description.push_str(
        "\n\nThis is a freeform tool: put the complete raw input text in the `input` \
         string property.",
    );
    let format = tool.get("format");
    let definition = format
        .and_then(|format| format.get("definition"))
        .and_then(Value::as_str)
        .filter(|definition| !definition.trim().is_empty());
    if let Some(definition) = definition {
        let syntax = format
            .and_then(|format| format.get("syntax"))
            .and_then(Value::as_str)
            .unwrap_or("grammar");
        description.push_str(&format!(
            " The input must conform to this {syntax} grammar:\n```\n{definition}\n```"
        ));
    }
    description
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn flattens_namespaces_reversibly() {
        assert_eq!(anthropic_tool_name(None, "shell"), "shell");
        assert_eq!(
            anthropic_tool_name(Some("mcp__github"), "create_issue"),
            "mcp__github__create_issue"
        );
    }

    #[test]
    fn rewrites_invalid_or_long_names_stably() {
        let dotted = anthropic_tool_name(None, "files.read");
        assert_eq!(dotted, anthropic_tool_name(None, "files.read"));
        assert!(dotted.starts_with("files_read_"));
        assert_ne!(dotted, anthropic_tool_name(None, "files_read"));

        let long = anthropic_tool_name(Some(&"n".repeat(40)), &"t".repeat(40));
        assert_eq!(long.len(), 64);
    }

    #[test]
    fn schemas_are_always_objects() {
        assert_eq!(
            object_schema(Some(&json!({"type": "string"}))),
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            object_schema(Some(&json!({"required": []}))),
            json!({"type": "object", "properties": {}, "required": []})
        );
        assert_eq!(
            object_schema(None),
            json!({"type": "object", "properties": {}})
        );
    }
}
