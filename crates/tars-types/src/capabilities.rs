//! Capability descriptor — what a Provider can do.
//!
//! Routing / Middleware layers consult this before sending a request.
//! Filling these in correctly is part of every Provider impl.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::usage::Pricing;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,

    pub supports_tool_use: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_structured_output: StructuredOutputMode,
    pub supports_vision: bool,
    pub supports_thinking: bool,
    pub supports_cancel: bool,

    pub prompt_cache: PromptCacheKind,
    pub streaming: bool,

    pub modalities_in: HashSet<Modality>,
    pub modalities_out: HashSet<Modality>,

    pub pricing: Pricing,
}

/// Validation error for `Capabilities::validate`.
#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error("modalities_in must be non-empty")]
    EmptyModalitiesIn,
    #[error("modalities_out must be non-empty")]
    EmptyModalitiesOut,
    #[error("supports_structured_output = ToolUseEmulation requires supports_tool_use = true")]
    ToolUseEmulationNeedsToolUse,
}

impl Capabilities {
    /// Reject internally inconsistent capability descriptors. Cheap;
    /// call once when a Provider is constructed. Audit findings
    /// `tars-types-src-capabilities-{9,14}`.
    pub fn validate(&self) -> Result<(), CapabilityError> {
        if self.modalities_in.is_empty() {
            return Err(CapabilityError::EmptyModalitiesIn);
        }
        if self.modalities_out.is_empty() {
            return Err(CapabilityError::EmptyModalitiesOut);
        }
        if matches!(
            self.supports_structured_output,
            StructuredOutputMode::ToolUseEmulation
        ) && !self.supports_tool_use
        {
            return Err(CapabilityError::ToolUseEmulationNeedsToolUse);
        }
        Ok(())
    }

    /// Minimal text-only chat with no extras. Useful baseline for tests
    /// and for adapters that intentionally turn features off.
    pub fn text_only_baseline(pricing: Pricing) -> Self {
        let mut modalities = HashSet::new();
        modalities.insert(Modality::Text);
        Self {
            max_context_tokens: 32_000,
            max_output_tokens: 4_096,
            supports_tool_use: false,
            supports_parallel_tool_calls: false,
            supports_structured_output: StructuredOutputMode::None,
            supports_vision: false,
            supports_thinking: false,
            supports_cancel: false,
            prompt_cache: PromptCacheKind::None,
            streaming: true,
            modalities_in: modalities.clone(),
            modalities_out: modalities,
            pricing,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutputMode {
    None,
    /// `{"type":"json_object"}` — JSON-shaped output but no schema enforcement.
    JsonObjectMode,
    /// Decode-time schema enforcement (OpenAI strict / Gemini responseSchema).
    StrictSchema,
    /// Anthropic-style: a forced `tool_choice` simulates strict output.
    ToolUseEmulation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheKind {
    None,
    /// Provider auto-caches long prefixes (OpenAI ≥1024 tokens).
    ImplicitPrefix {
        min_tokens: u32,
    },
    /// Inline `cache_control` markers (Anthropic).
    ExplicitMarker,
    /// Out-of-band `cachedContent` API (Gemini).
    ExplicitObject,
    /// Cache lives in the binary we shell out to (CLI providers).
    Delegated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_baseline_has_text_modalities() {
        let caps = Capabilities::text_only_baseline(Pricing::default());
        assert!(caps.modalities_in.contains(&Modality::Text));
        assert!(caps.modalities_out.contains(&Modality::Text));
        assert_eq!(caps.modalities_in.len(), 1);
    }

    #[test]
    fn validate_rejects_empty_modalities() {
        let mut caps = Capabilities::text_only_baseline(Pricing::default());
        caps.modalities_in.clear();
        assert!(matches!(
            caps.validate(),
            Err(CapabilityError::EmptyModalitiesIn)
        ));

        let mut caps = Capabilities::text_only_baseline(Pricing::default());
        caps.modalities_out.clear();
        assert!(matches!(
            caps.validate(),
            Err(CapabilityError::EmptyModalitiesOut)
        ));
    }

    #[test]
    fn validate_rejects_tool_use_emulation_without_tool_use() {
        let mut caps = Capabilities::text_only_baseline(Pricing::default());
        caps.supports_structured_output = StructuredOutputMode::ToolUseEmulation;
        // supports_tool_use is false from the baseline.
        assert!(matches!(
            caps.validate(),
            Err(CapabilityError::ToolUseEmulationNeedsToolUse)
        ));

        caps.supports_tool_use = true;
        assert!(caps.validate().is_ok());
    }

    #[test]
    fn baseline_validates_clean() {
        assert!(
            Capabilities::text_only_baseline(Pricing::default())
                .validate()
                .is_ok()
        );
    }
}
