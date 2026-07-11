//! Capability descriptor — what a Provider can do.
//!
//! Routing / Middleware layers consult this before sending a request.
//! Filling these in correctly is part of every Provider impl.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::usage::Pricing;

/// How tars reaches and DRIVES a provider — the interface, not the wire dialect.
///
/// Two axes are folded into this one label, and NEITHER is observable at runtime,
/// so it is always DECLARED (a total match over `ProviderConfig` variants), never
/// detected:
///   - who runs the agent loop: `Cli` = the vendor binary runs its own loop with
///     its own tools and edits the worktree directly (tars hands a prompt, gets
///     final text, MUST NOT hand it a tool registry); every other kind = tars
///     drives the loop.
///   - what the transport is: `Http` = HTTP tars constructs itself; `Api` = a
///     vendor SDK / long-lived daemon (claude_sdk over NDJSON, bedrock over the
///     AWS SDK). `claude_cli` and `mlx` are BOTH "local processes" yet land in
///     different kinds — locality is a separate axis this does not model.
///
/// Contrast arc's deleted `ProviderType::from_wire`, whose `_ => Other` silently
/// misfiled `opencode`/`antigravity`: this enum has no catch-all, so a new variant
/// fails to compile until its interface is declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceKind {
    /// A vendor agent binary that runs its OWN loop with its OWN tools.
    /// {claude_cli, gemini_cli, codex_cli, opencode, antigravity}
    Cli,
    /// tars drives the loop and supplies the tools, over HTTP it constructs.
    /// {openai, anthropic, gemini, deepseek, xai, vllm, mlx, llamacpp}
    Http,
    /// tars drives the loop, but the wire is a vendor SDK / daemon, not raw HTTP
    /// tars builds. {claude_sdk (Node NDJSON daemon), bedrock (aws_sdk, SigV4)}
    Api,
    /// No call is placed. {mock, cassette}
    Mock,
}

fn default_interface() -> InterfaceKind {
    InterfaceKind::Http
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    /// How tars reaches and drives this provider (declared, never detected).
    ///
    /// Defaults to `Http` when absent so serialized descriptors predating this
    /// field (committed cassette files) still deserialize — `interface` is
    /// cosmetic on a replayed cassette.
    #[serde(default = "default_interface")]
    pub interface: InterfaceKind,

    /// Maximum prompt-context window. `None` = no ceiling to enforce (the
    /// provider imposes no documented limit, or it is genuinely unknown — the
    /// data file's comment tells a human which). Readers that enforce a ceiling
    /// MUST treat `None` as "don't enforce".
    pub max_context_tokens: Option<u32>,
    /// Maximum tokens the caller may request as output. `None` = no ceiling
    /// (see [`Self::max_context_tokens`]).
    pub max_output_tokens: Option<u32>,

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
    #[error("supports_parallel_tool_calls = true requires supports_tool_use = true")]
    ParallelToolCallsNeedsToolUse,
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
        if self.supports_parallel_tool_calls && !self.supports_tool_use {
            return Err(CapabilityError::ParallelToolCallsNeedsToolUse);
        }
        Ok(())
    }

    /// Minimal text-only chat with no extras. Useful baseline for tests
    /// and for adapters that intentionally turn features off.
    pub fn text_only_baseline(pricing: Pricing) -> Self {
        let mut modalities = HashSet::new();
        modalities.insert(Modality::Text);
        Self {
            // The baseline is the fallback for a provider the data file doesn't
            // name (an anonymous `openai_compat` a user pointed at a local
            // server). Those are HTTP tars drives itself.
            interface: InterfaceKind::Http,
            max_context_tokens: Some(32_000),
            max_output_tokens: Some(4_096),
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
    /// The `json_object` alias lets `data/provider.toml` spell it the way the
    /// wire does without changing this enum's serialized form.
    #[serde(alias = "json_object")]
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
