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

/// Name of the Anthropic web search server tool.
pub(crate) const WEB_SEARCH_TOOL_NAME: &str = "web_search";

/// Translates the Responses `tools` array into Anthropic tool definitions and
/// records how to map Anthropic tool calls back to Codex items.
///
/// The output is deterministic for a given tool set, so it can sit in the
/// cached prompt prefix: client tools are sorted by name, JSON keys are
/// sorted, and the web search server tool (when Codex declares `web_search`)
/// comes last, as `web_search_type`.
pub(crate) fn translate_tools(
    tools: &[Value],
    registry: &mut ToolRegistry,
    web_search_type: &str,
) -> Vec<Value> {
    let mut out = Vec::new();
    let mut web_search = None;
    for tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("web_search" | "web_search_preview") => {
                if web_search.is_none() {
                    web_search = Some(web_search_tool(tool, web_search_type));
                }
            }
            _ => translate_tool(tool, /*namespace*/ None, registry, &mut out),
        }
    }
    out.sort_by(|a, b| tool_name(a).cmp(tool_name(b)));
    let mut out: Vec<Value> = out.iter().map(canonical_json).collect();
    if let Some(web_search) = web_search {
        if registry.lookup(WEB_SEARCH_TOOL_NAME).is_some() {
            debug!("a client tool is named web_search; not adding the web search server tool");
        } else {
            out.push(canonical_json(&web_search));
        }
    }
    out
}

fn tool_name(tool: &Value) -> &str {
    tool.get("name").and_then(Value::as_str).unwrap_or_default()
}

/// Maps Codex's hosted `web_search` tool to Anthropic's web search server
/// tool. Anthropic search is always live: Codex's cached mode
/// (`external_web_access: false`) has no equivalent.
fn web_search_tool(tool: &Value, web_search_type: &str) -> Value {
    let mut definition = json!({"type": web_search_type, "name": WEB_SEARCH_TOOL_NAME});
    let allowed_domains: Vec<Value> = tool
        .get("filters")
        .and_then(|filters| filters.get("allowed_domains"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|domain| domain.as_str().is_some_and(|domain| !domain.is_empty()))
        .cloned()
        .collect();
    if !allowed_domains.is_empty() {
        definition["allowed_domains"] = Value::Array(allowed_domains);
    }
    if let Some(location) = tool.get("user_location").and_then(Value::as_object) {
        let mut user_location = Map::new();
        for key in ["city", "region", "country", "timezone"] {
            if let Some(value) = location
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                user_location.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
        if !user_location.is_empty() {
            user_location.insert("type".to_string(), Value::String("approximate".to_string()));
            definition["user_location"] = Value::Object(user_location);
        }
    }
    definition
}

/// Rebuilds a JSON value with every object's keys in sorted order, so the
/// serialized bytes do not depend on how the input map was ordered.
pub(crate) fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonical_json(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
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
    fn tools_are_sorted_and_web_search_becomes_a_server_tool() {
        let function = |name: &str| json!({"type": "function", "name": name, "parameters": {"type": "object", "properties": {"b": {"type": "string"}, "a": {"type": "string"}}}});
        let web_search = json!({
            "type": "web_search",
            "external_web_access": true,
            "filters": {"allowed_domains": ["docs.rs"]},
            "user_location": {"type": "approximate", "country": "US", "city": null},
        });
        let mut registry = ToolRegistry::default();
        let first = translate_tools(
            &[web_search.clone(), function("zeta"), function("alpha")],
            &mut registry,
            "web_search_20260209",
        );
        let second = translate_tools(
            &[function("alpha"), function("zeta"), web_search],
            &mut ToolRegistry::default(),
            "web_search_20260209",
        );
        assert_eq!(
            serde_json::to_string(&first).ok(),
            serde_json::to_string(&second).ok()
        );
        let names: Vec<&str> = first.iter().map(tool_name).collect();
        assert_eq!(names, vec!["alpha", "zeta", "web_search"]);
        assert_eq!(
            first[2],
            json!({
                "type": "web_search_20260209",
                "name": "web_search",
                "allowed_domains": ["docs.rs"],
                "user_location": {"type": "approximate", "country": "US"},
            })
        );
        assert_eq!(registry.lookup("web_search"), None);
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let mut map = Map::new();
        map.insert("z".to_string(), json!(1));
        map.insert("a".to_string(), json!({"y": 2, "b": 3}));
        assert_eq!(
            canonical_json(&Value::Object(map)).to_string(),
            r#"{"a":{"b":3,"y":2},"z":1}"#
        );
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
