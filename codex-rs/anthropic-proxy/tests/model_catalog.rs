//! Checks the shipped example `model_catalog_json` file.

use pretty_assertions::assert_eq;
use serde_json::Value;

const CATALOG: &str = include_str!("../model_catalog.example.json");

/// `ModelInfo` fields without serde defaults (protocol/src/openai_models.rs),
/// plus the legacy `base_instructions` the catalog loader requires.
const REQUIRED_FIELDS: &[&str] = &[
    "slug",
    "display_name",
    "description",
    "supported_reasoning_levels",
    "shell_type",
    "visibility",
    "supported_in_api",
    "priority",
    "availability_nux",
    "upgrade",
    "support_verbosity",
    "default_verbosity",
    "apply_patch_tool_type",
    "truncation_policy",
    "experimental_supported_tools",
    "base_instructions",
];

#[test]
fn example_catalog_lists_the_claude_models() {
    let catalog: Value = serde_json::from_str(CATALOG).unwrap();
    let models = catalog["models"].as_array().unwrap();
    let slugs: Vec<&str> = models
        .iter()
        .map(|model| model["slug"].as_str().unwrap())
        .collect();
    assert_eq!(
        slugs,
        vec![
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-2"
        ]
    );
    for model in models {
        for field in REQUIRED_FIELDS {
            assert!(
                model.get(*field).is_some(),
                "{} lacks {field}",
                model["slug"]
            );
        }
        assert!(!model["base_instructions"].as_str().unwrap().is_empty());
        assert_eq!(model["apply_patch_tool_type"], "freeform");
        assert_eq!(model["supports_search_tool"], false);
    }
}
