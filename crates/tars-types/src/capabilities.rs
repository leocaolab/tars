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

impl Capabilities {
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
    ImplicitPrefix { min_tokens: u32 },
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
}
