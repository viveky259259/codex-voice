use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn reported() -> Vec<Value> {
    vec![
        json!({"value": "default", "displayName": "Default (recommended)", "description": "Use the default model"}),
        json!({"value": "opus", "resolvedModel": "claude-opus-5-5", "displayName": "Opus", "description": "Opus 5.5",
               "supportsEffort": true, "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]}),
        json!({"value": "sonnet[1m]", "resolvedModel": "claude-sonnet-5-5[1m]", "displayName": "Sonnet (1M context)", "description": ""}),
    ]
}

#[test]
fn init_models_come_first_and_the_fallback_fills_in() {
    let models = merge_models(Some(&reported()));
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "default",
            "opus",
            "sonnet[1m]",
            "claude-fable-5-2",
            "claude-haiku-5-5",
        ]
    );
    assert_eq!(models[1].display_name, "Opus");
    assert_eq!(
        models[1].efforts,
        vec!["low", "medium", "high", "xhigh", "max"]
    );
    assert!(models[1].reported);
    assert!(!models[3].reported);
}

#[test]
fn without_init_models_the_fallback_is_the_list() {
    let ids: Vec<String> = merge_models(/*reported*/ None)
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(
        ids,
        vec![
            "claude-fable-5-2",
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
        ]
    );
}

#[test]
fn each_family_name_belongs_to_its_first_model() {
    assert_eq!(
        family_aliases([
            "default",
            "opus",
            "claude-opus-5-5",
            "sonnet[1m]",
            "claude-haiku-5-5",
            "claude-fable-5-2"
        ]),
        vec![
            None,
            Some("opus"),
            None,
            Some("sonnet"),
            Some("haiku"),
            Some("fable")
        ]
    );
}

#[test]
fn resolve_by_id_resolved_id_or_family() {
    let models = merge_models(Some(&reported()));
    assert_eq!(
        resolve(&models, "opus").map(|m| m.id.as_str()),
        Some("opus")
    );
    assert_eq!(
        resolve(&models, "claude-opus-5-5").map(|m| m.id.as_str()),
        Some("opus")
    );
    assert_eq!(
        resolve(&models, "sonnet").map(|m| m.id.as_str()),
        Some("sonnet[1m]")
    );
    assert_eq!(
        resolve(&models, "fable").map(|m| m.id.as_str()),
        Some("claude-fable-5-2")
    );
    assert_eq!(resolve(&models, "gpt-5.1"), None);
}

#[test]
fn only_claude_models_are_passed_to_the_cli() {
    for id in [
        "claude-opus-5-5",
        "opus",
        "sonnet[1m]",
        "default",
        "us.anthropic.claude-opus-5-5-v1",
    ] {
        assert!(is_claude_model_id(id), "{id}");
    }
    for id in ["gpt-5.1", "o3", "gpt-oss:20b"] {
        assert!(!is_claude_model_id(id), "{id}");
    }
}

#[test]
fn protocol_models_carry_names_efforts_and_the_current_model() {
    let models = protocol_models(&merge_models(Some(&reported())), "opus");
    let opus = &models[1];
    assert_eq!(
        (
            opus.model.as_str(),
            opus.display_name.as_str(),
            opus.is_default
        ),
        ("opus", "Opus", true)
    );
    assert_eq!(opus.supported_reasoning_efforts.len(), 5);
    assert_eq!(opus.default_reasoning_effort, ReasoningEffort::Medium);
    assert!(!models[0].is_default);
}
