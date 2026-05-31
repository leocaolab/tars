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
    ///
    /// **Nesting is bounded.** `Ensemble` is recursive, so a maliciously
    /// or accidentally deeply-nested document could blow the stack
    /// during the derived (recursive) `Deserialize`. After deserializing,
    /// callers that accept untrusted input MUST call
    /// [`ModelHint::validate_depth`] (or [`ModelHint::depth`]) to reject
    /// pathological nesting before the value is walked recursively
    /// elsewhere. Routing's expansion never legitimately nests beyond a
    /// couple of levels, so [`ModelHint::MAX_DEPTH`] is the sane cap.
    Ensemble(Vec<ModelHint>),
}

impl ModelHint {
    /// Maximum legitimate `Ensemble` nesting depth. A flat hint is
    /// depth 1; `Ensemble([Explicit])` is depth 2. Anything deeper than
    /// this is rejected by [`validate_depth`](Self::validate_depth) —
    /// real routing configs never nest ensembles more than a level or
    /// two, so this is generous while still bounding stack use.
    pub const MAX_DEPTH: usize = 8;

    /// Maximum nesting depth of this hint. A non-`Ensemble` hint is 1;
    /// an `Ensemble` is `1 + max(child depth)` (empty ensemble = 1).
    pub fn depth(&self) -> usize {
        match self {
            Self::Ensemble(children) => {
                1 + children.iter().map(Self::depth).max().unwrap_or(0)
            }
            _ => 1,
        }
    }

    /// Reject hints whose `Ensemble` nesting exceeds [`Self::MAX_DEPTH`].
    /// Call this on any `ModelHint` parsed from untrusted input before
    /// processing it recursively.
    pub fn validate_depth(&self) -> Result<(), &'static str> {
        if self.depth() > Self::MAX_DEPTH {
            Err("ModelHint::Ensemble nesting exceeds MAX_DEPTH")
        } else {
            Ok(())
        }
    }

    /// Construct an `Explicit` hint, rejecting empty / whitespace-only
    /// model names. The bare `ModelHint::Explicit(s)` variant stays
    /// public (serde + exhaustive matching across layers depend on it),
    /// but callers building a hint from user/config input should prefer
    /// this so a blank model name fails fast here rather than surfacing
    /// as an opaque provider 404 (or panicking in
    /// `ProviderId::new(label())`, which rejects empty strings).
    pub fn try_explicit(name: impl Into<String>) -> Result<Self, &'static str> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err("ModelHint::Explicit model name cannot be empty/whitespace");
        }
        Ok(Self::Explicit(name))
    }

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
    ///
    /// Returns [`Cow`] so the common `Explicit` case borrows the stored
    /// model name instead of cloning it — model names can be long, and
    /// `label()` is called on hot paths (per-request logging, cache-key
    /// building). The `Tier` / `Ensemble` cases still allocate the
    /// formatted string.
    pub fn label(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Explicit(s) => std::borrow::Cow::Borrowed(s.as_str()),
            Self::Tier(t) => std::borrow::Cow::Owned(format!("tier:{:?}", t).to_lowercase()),
            Self::Ensemble(models) => {
                std::borrow::Cow::Owned(format!("ensemble:{}", models.len()))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
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
        assert_eq!(ModelHint::Explicit("gpt-4o".into()).label().as_ref(), "gpt-4o");
        assert_eq!(
            ModelHint::Tier(ModelTier::Reasoning).label().as_ref(),
            "tier:reasoning"
        );
        assert_eq!(
            ModelHint::Ensemble(vec![ModelHint::Tier(ModelTier::Fast); 3]).label().as_ref(),
            "ensemble:3"
        );
    }

    #[test]
    fn explicit_returns_none_for_non_explicit() {
        assert!(ModelHint::Tier(ModelTier::Fast).explicit().is_none());
        assert_eq!(ModelHint::Explicit("x".into()).explicit(), Some("x"));
    }
}
