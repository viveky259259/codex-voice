//! Per-model request rules for the Anthropic Messages API.
//!
//! Models are matched by slug (`claude-<family>-<major>-<minor>`, optionally
//! provider-prefixed, e.g. `anthropic.claude-opus-5-5`). Slugs this table does
//! not recognize get the rules of the newest models, which are the strictest:
//! thinking always on, no forced `tool_choice`, no sampling parameters.

/// Web search server tool version for current models.
pub(crate) const WEB_SEARCH_TOOL_CURRENT: &str = "web_search_20260209";
/// Web search server tool version for models older than the 4.6 family.
pub(crate) const WEB_SEARCH_TOOL_BASIC: &str = "web_search_20250305";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFamily {
    /// Claude Fable 5.x and Mythos 5.x.
    Fable5,
    Opus55,
    Sonnet55,
    Haiku55,
    Opus5,
    Sonnet5,
    /// Opus 4.7 and 4.8.
    Opus48,
    /// Opus 4.6 and Sonnet 4.6.
    Claude46,
    /// Haiku 4.5, the other 4.0 – 4.5 models, and Claude 3.7 Sonnet.
    Budget,
    /// Claude 3.5 and older: no extended thinking.
    Legacy,
    /// An unrecognized slug; gets the newest models' rules.
    Unknown,
}

/// How a model takes the `thinking` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Thinking {
    /// `{type: "adaptive"}`; omitting `thinking` still thinks. Some of these
    /// models reject `{type: "disabled"}` (Fable 5.x, Opus 5.5, Sonnet 5.5) or
    /// accept it only at effort `high` or below (Opus 5, Haiku 5.5), so the
    /// proxy never sends it: Codex's `none` effort becomes `effort: "low"`.
    AdaptiveOnByDefault,
    /// `{type: "adaptive"}`; omitting `thinking` means no thinking (Opus
    /// 4.8/4.7, the 4.6 family), so `none` effort omits it.
    AdaptiveOffByDefault,
    /// `{type: "enabled", budget_tokens}`.
    Budget,
    /// No extended thinking.
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelRules {
    pub(crate) family: ModelFamily,
    pub(crate) thinking: Thinking,
    /// Accepts `output_config.effort: "xhigh"` (otherwise `xhigh` → `max`).
    pub(crate) supports_xhigh: bool,
    /// Accepts forced `tool_choice` (`any` / `tool`) when thinking is off.
    /// Forced choice is rejected by every model while thinking is on.
    pub(crate) forced_tool_choice: bool,
    /// Accepts `temperature` / `top_p` (only forwarded while thinking is off).
    pub(crate) sampling: bool,
    /// `type` of the web search server tool.
    pub(crate) web_search_tool: &'static str,
}

impl ModelRules {
    const fn new(family: ModelFamily) -> Self {
        use ModelFamily::*;
        let thinking = match family {
            Fable5 | Opus55 | Sonnet55 | Haiku55 | Opus5 | Sonnet5 | Unknown => {
                Thinking::AdaptiveOnByDefault
            }
            Opus48 | Claude46 => Thinking::AdaptiveOffByDefault,
            Budget => Thinking::Budget,
            Legacy => Thinking::Unsupported,
        };
        Self {
            family,
            thinking,
            supports_xhigh: !matches!(family, Claude46),
            forced_tool_choice: !matches!(family, Fable5 | Opus55 | Sonnet55 | Unknown),
            // Sonnet 5.5 and Haiku 5.5 reject non-default values, the other
            // 5.x and 4.7+ models reject the parameters outright.
            sampling: matches!(family, Claude46 | Budget | Legacy),
            web_search_tool: match family {
                Haiku55 | Budget | Legacy => WEB_SEARCH_TOOL_BASIC,
                _ => WEB_SEARCH_TOOL_CURRENT,
            },
        }
    }

    /// True when the model thinks for this request: either `thinking` is set
    /// or the model thinks by default.
    pub(crate) fn thinks(&self, thinking_param_set: bool) -> bool {
        thinking_param_set || self.thinking == Thinking::AdaptiveOnByDefault
    }
}

pub(crate) fn model_rules(model: &str) -> ModelRules {
    ModelRules::new(model_family(model))
}

pub(crate) fn model_family(model: &str) -> ModelFamily {
    // Accept provider-prefixed ids such as `anthropic.claude-...`.
    let Some(start) = model.find("claude-") else {
        return ModelFamily::Unknown;
    };
    let parts: Vec<&str> = model[start + "claude-".len()..].split('-').collect();
    // Legacy naming: claude-3-7-sonnet, claude-3-5-haiku, claude-3-opus.
    if let Some(major) = parts.first().and_then(|part| part.parse::<u32>().ok()) {
        let minor = parts.get(1).and_then(|part| part.parse::<u32>().ok());
        return match (major, minor) {
            (3, Some(7)) => ModelFamily::Budget,
            (0..=3, _) => ModelFamily::Legacy,
            _ => ModelFamily::Unknown,
        };
    }
    let family = parts.first().copied().unwrap_or_default();
    let Some(major) = parts.get(1).and_then(|part| part.parse::<u32>().ok()) else {
        return ModelFamily::Unknown;
    };
    // A dated snapshot (`claude-sonnet-4-20250514`) has no minor version.
    let minor = parts
        .get(2)
        .filter(|part| part.len() <= 2)
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    match (family, major, minor) {
        ("fable" | "mythos", 5, 0..=2) => ModelFamily::Fable5,
        ("opus", 5, 5) => ModelFamily::Opus55,
        ("sonnet", 5, 5) => ModelFamily::Sonnet55,
        ("haiku", 5, 5) => ModelFamily::Haiku55,
        ("opus", 5, 0) => ModelFamily::Opus5,
        ("sonnet", 5, 0) => ModelFamily::Sonnet5,
        ("opus", 4, 7 | 8) => ModelFamily::Opus48,
        ("opus" | "sonnet", 4, 6) => ModelFamily::Claude46,
        ("opus" | "sonnet" | "haiku", 4, 0..=5) => ModelFamily::Budget,
        _ => ModelFamily::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn families_by_slug() {
        use ModelFamily::*;
        for (model, family) in [
            ("claude-fable-5-2", Fable5),
            ("claude-fable-5-1", Fable5),
            ("claude-mythos-5-2", Fable5),
            ("claude-opus-5-5", Opus55),
            ("anthropic.claude-opus-5-5", Opus55),
            ("claude-sonnet-5-5", Sonnet55),
            ("claude-haiku-5-5", Haiku55),
            ("claude-opus-5", Opus5),
            ("claude-sonnet-5", Sonnet5),
            ("claude-opus-4-8", Opus48),
            ("claude-opus-4-7", Opus48),
            ("claude-opus-4-6", Claude46),
            ("claude-sonnet-4-6", Claude46),
            ("claude-haiku-4-5", Budget),
            ("claude-sonnet-4-5", Budget),
            ("claude-opus-4-1-20250805", Budget),
            ("claude-sonnet-4-20250514", Budget),
            ("claude-3-7-sonnet-latest", Budget),
            ("claude-3-5-haiku-latest", Legacy),
            // Unknown and future slugs get the newest rules.
            ("claude-opus-5-6", Unknown),
            ("claude-fable-6", Unknown),
            ("claude-sonnet-4-9", Unknown),
            ("some-custom-alias", Unknown),
        ] {
            assert_eq!(model_family(model), family, "{model}");
        }
    }

    #[test]
    fn unknown_models_get_the_strictest_rules() {
        let rules = model_rules("claude-next-1");
        assert_eq!(rules.thinking, Thinking::AdaptiveOnByDefault);
        assert!(!rules.forced_tool_choice);
        assert!(!rules.sampling);
        assert_eq!(rules.web_search_tool, WEB_SEARCH_TOOL_CURRENT);
    }
}
