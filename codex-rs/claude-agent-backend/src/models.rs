//! The Claude model list: what the CLI reports in its `initialize` response
//! (`models: ModelInfo[]`), merged with a small built-in fallback, the way
//! T3 Code merges its model manifest with `init.models`.

use codex_app_server_protocol::Model;
use codex_app_server_protocol::ReasoningEffortOption;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ReasoningEffort;
use serde_json::Value;
use std::str::FromStr;

/// Claude Code's pointer at its own default model.
pub const DEFAULT_MODEL: &str = "default";

/// Spoken names, each resolving to the first listed model of that family.
pub const FAMILIES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

/// Used when the CLI reports no models, and merged in when it reports some.
pub const FALLBACK_MODELS: [(&str, &str); 4] = [
    ("claude-fable-5-2", "Claude Fable 5.2"),
    ("claude-opus-5-5", "Claude Opus 5.5"),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
    ("claude-haiku-5-5", "Claude Haiku 5.5"),
];

const FALLBACK_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeModel {
    /// The value passed to `--model` and `set_model`.
    pub id: String,
    /// The canonical id an alias row resolves to, when the CLI says so.
    pub resolved: Option<String>,
    pub display_name: String,
    pub description: String,
    /// Effort levels, when the CLI reports them (`low`, `medium`, ... `max`).
    pub efforts: Vec<String>,
    /// Whether this row came from the CLI rather than the fallback list.
    pub reported: bool,
}

/// `ModelInfo` rows first (in the CLI's order), then every fallback model the
/// CLI did not already list under its id or resolved id.
pub fn merge_models(reported: Option<&[Value]>) -> Vec<ClaudeModel> {
    let mut models: Vec<ClaudeModel> = Vec::new();
    for info in reported.unwrap_or_default() {
        let Some(id) = info.get("value").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if id.is_empty() || models.iter().any(|model| model.id == id) {
            continue;
        }
        let text = |key: &str| {
            info.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        let supports_effort = info.get("supportsEffort").and_then(Value::as_bool);
        let efforts = if supports_effort == Some(false) {
            Vec::new()
        } else {
            info.get("supportedEffortLevels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        };
        models.push(ClaudeModel {
            id: id.to_string(),
            resolved: text("resolvedModel"),
            display_name: text("displayName").unwrap_or_else(|| id.to_string()),
            description: text("description").unwrap_or_default(),
            efforts,
            reported: true,
        });
    }
    let known: Vec<String> = models
        .iter()
        .flat_map(|model| [Some(model.id.as_str()), model.resolved.as_deref()])
        .flatten()
        .map(base_id)
        .collect();
    for (id, display_name) in FALLBACK_MODELS {
        if known.iter().any(|known| known == id) {
            continue;
        }
        models.push(ClaudeModel {
            id: id.to_string(),
            resolved: None,
            display_name: display_name.to_string(),
            description: format!("{display_name} (built-in list)"),
            efforts: FALLBACK_EFFORTS
                .iter()
                .map(|effort| effort.to_string())
                .collect(),
            reported: false,
        });
    }
    models
}

/// Lowercase, without a context-window suffix such as `[1m]`.
fn base_id(id: &str) -> String {
    let id = id.trim();
    let id = match id.rfind('[') {
        Some(start) if id.ends_with(']') => &id[..start],
        _ => id,
    };
    id.trim().to_ascii_lowercase()
}

/// The family a model id or display name belongs to, if any.
pub fn family_of(text: &str) -> Option<&'static str> {
    let text = text.to_ascii_lowercase();
    FAMILIES.into_iter().find(|family| text.contains(family))
}

/// For each slug, the family name it is spoken by: only the first model of
/// each family gets one, so "switch to opus" is never ambiguous. The default
/// pointer gets none.
pub fn family_aliases<'a>(slugs: impl IntoIterator<Item = &'a str>) -> Vec<Option<&'static str>> {
    let mut taken: Vec<&'static str> = Vec::new();
    slugs
        .into_iter()
        .map(|slug| {
            if slug == DEFAULT_MODEL {
                return None;
            }
            let family = family_of(slug).filter(|family| !taken.contains(family))?;
            taken.push(family);
            Some(family)
        })
        .collect()
}

/// Whether a configured model is meant for Claude rather than for Codex.
pub fn is_claude_model_id(id: &str) -> bool {
    let id = base_id(id);
    id == DEFAULT_MODEL
        || id == "opusplan"
        || id.starts_with("claude")
        || id.contains("anthropic")
        || FAMILIES.contains(&id.as_str())
}

/// Find a model by id, resolved id or family name.
pub fn resolve<'a>(models: &'a [ClaudeModel], requested: &str) -> Option<&'a ClaudeModel> {
    let wanted = base_id(requested);
    models
        .iter()
        .find(|model| base_id(&model.id) == wanted)
        .or_else(|| {
            models
                .iter()
                .find(|model| model.resolved.as_deref().map(base_id) == Some(wanted.clone()))
        })
        .or_else(|| {
            let index = family_aliases(models.iter().map(|model| model.id.as_str()))
                .into_iter()
                .position(|family| family == Some(wanted.as_str()))?;
            models.get(index)
        })
}

/// The app-server `model/list` page for these models. `current` is marked default.
pub fn protocol_models(models: &[ClaudeModel], current: &str) -> Vec<Model> {
    models
        .iter()
        .map(|model| {
            let efforts: Vec<ReasoningEffortOption> = model
                .efforts
                .iter()
                .filter_map(|effort| {
                    Some(ReasoningEffortOption {
                        reasoning_effort: ReasoningEffort::from_str(effort).ok()?,
                        description: effort_label(effort).to_string(),
                    })
                })
                .collect();
            let default_effort = efforts
                .iter()
                .map(|option| option.reasoning_effort.clone())
                .find(|effort| *effort == ReasoningEffort::Medium)
                .or_else(|| {
                    efforts
                        .first()
                        .map(|option| option.reasoning_effort.clone())
                })
                .unwrap_or_default();
            Model {
                id: model.id.clone(),
                model: model.id.clone(),
                upgrade: None,
                upgrade_info: None,
                availability_nux: None,
                display_name: model.display_name.clone(),
                description: model.description.clone(),
                model_specialty: None,
                hidden: false,
                supported_reasoning_efforts: efforts,
                default_reasoning_effort: default_effort,
                input_modalities: vec![InputModality::Text, InputModality::Image],
                supports_personality: false,
                multi_agent_version: None,
                additional_speed_tiers: Vec::new(),
                service_tiers: Vec::new(),
                default_service_tier: None,
                available_access_programs: None,
                is_default: model.id == current,
            }
        })
        .collect()
}

fn effort_label(effort: &str) -> &str {
    match effort {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        other => other,
    }
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
