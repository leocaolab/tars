//! Model selection abstractions.
//!
//! The Provider layer accepts `ModelHint::Tier(...)` (abstract) or
//! `ModelHint::Explicit(...)` (concrete). Routing layer (Doc 02 §4.6)
//! resolves Tier → Explicit. Cache layer requires Explicit before
//! computing a key (Doc 03 §4.2).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ModelHint {
    /// Concrete provider-side model name (e.g. `"gpt-4o-2024-08-06"`).
    Explicit(String),
    /// Abstract tier — let the routing policy pick a concrete model.
    Tier(ModelTier),
    /// Multiple models in parallel; merge results per policy.
    /// (Not used by Provider directly — Routing layer expands this.)
    Ensemble(Vec<ModelHint>),
}

impl ModelHint {
    /// Returns the concrete model string if this is `Explicit`,
    /// `None` otherwise. Provider implementations call this and refuse
    /// to handle non-explicit hints.
    pub fn explicit(&self) -> Option<&str> {
        match self {
            Self::Explicit(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Diagnostic-friendly label for logs/audit. Always returns a
    /// short string, never None.
    pub fn label(&self) -> String {
        match self {
            Self::Explicit(s) => s.clone(),
            Self::Tier(t) => format!("tier:{:?}", t).to_lowercase(),
            Self::Ensemble(models) => {
                format!("ensemble:{}", models.len())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// Top-of-the-line reasoning models (Claude Opus, o1, Gemini Pro).
    Reasoning,
    /// Default workhorse (Sonnet, GPT-4o, Flash).
    Default,
    /// Fast / cheap classification + routing (4o-mini, Haiku, Flash-8B).
    Fast,
    /// Local model (Qwen, Llama, etc).
    Local,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ThinkingMode {
    /// Disable any provider "thinking" feature.
    #[default]
    Off,
    /// Let the provider auto-decide thinking depth.
    Auto,
    /// Hard cap on thinking tokens (Anthropic-style).
    Budget(u32),
}

impl ThinkingMode {
    pub fn is_off(&self) -> bool {
        matches!(self, Self::Off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_hint_label_is_diagnostic() {
        assert_eq!(ModelHint::Explicit("gpt-4o".into()).label(), "gpt-4o");
        assert_eq!(ModelHint::Tier(ModelTier::Reasoning).label(), "tier:reasoning");
        assert_eq!(
            ModelHint::Ensemble(vec![ModelHint::Tier(ModelTier::Fast); 3]).label(),
            "ensemble:3"
        );
    }

    #[test]
    fn explicit_returns_none_for_non_explicit() {
        assert!(ModelHint::Tier(ModelTier::Fast).explicit().is_none());
        assert_eq!(
            ModelHint::Explicit("x".into()).explicit(),
            Some("x")
        );
    }
}
