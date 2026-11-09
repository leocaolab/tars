//! Describes what a provider can do and how tars reaches it, for routing and middleware to
//! consult before sending a request.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::providers::usage::Pricing;

/// How tars reaches and drives a provider: the interface, not the wire dialect.
///
/// It folds two axes, neither observable at run time, so it is declared by an exhaustive match
/// over the `ProviderConfig` variants (tars-config's `ProviderConfig::interface`) and never
/// detected:
///   - who runs the agent loop: under `Cli` the vendor binary runs its own loop with its own
///     tools and edits the worktree itself, so tars hands it a prompt and must not hand it a tool
///     registry; under every other kind tars drives the loop.
///   - the transport: `Http` is HTTP tars builds itself; `Api` is a vendor SDK or long-lived
///     daemon. Locality is not modelled: `claude_cli` and `mlx` are both local processes and land
///     in different kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceKind {
    /// A vendor agent binary that runs its own loop with its own tools (claude_cli, gemini_cli,
    /// codex_cli, opencode, antigravity).
    Cli,
    /// tars drives the loop and supplies the tools over HTTP it builds (openai, anthropic,
    /// gemini, deepseek, xai, vllm, mlx, llamacpp).
    Http,
    /// tars drives the loop over a vendor SDK or daemon: claude_sdk (a Node NDJSON daemon).
    Api,
    /// No call is placed (mock, cassette).
    Mock,
}

fn default_interface() -> InterfaceKind {
    InterfaceKind::Http
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// Defaults to `Http` when absent, so committed cassette files written without it still
    /// load; a replayed cassette does not read it.
    #[serde(default = "default_interface")]
    pub interface: InterfaceKind,
    /// Names who serves the API in OpenTelemetry GenAI's words for `gen_ai.provider.name`
    /// (`openai`, `anthropic`, `gcp.gemini`, `deepseek`); `None` when tars does not know, as for a
    /// local server or an `openai_compat` block that names none. It is not the model's vendor:
    /// Qwen served by a local vLLM is `vllm`.
    #[serde(default)]
    pub gen_ai_provider_name: Option<String>,

    /// The prompt-context window in tokens; `None` means no ceiling to enforce, either because
    /// the provider documents none or because it is unknown.
    pub max_context_tokens: Option<u32>,
    /// The most output tokens a caller may request; `None` means no ceiling to enforce.
    pub max_output_tokens: Option<u32>,

    pub supports_tool_use: Support,
    pub supports_parallel_tool_calls: bool,
    /// The mechanism that holds output to a schema; `None` when the profile has no data on it,
    /// written `null` as an unknown [`Support`] is.
    pub supports_structured_output: Option<StructuredOutputMode>,
    pub supports_vision: Support,
    pub supports_thinking: Support,
    /// Whether the server reads the chat template's thinking switch
    /// (`chat_template_kwargs.enable_thinking`), as vLLM, mlx_lm.server and llama-server do; a
    /// vendor API has no chat template. Absent in a profile written without it, it is `false`.
    #[serde(default)]
    pub supports_chat_template_thinking: bool,
    pub supports_cancel: bool,

    pub prompt_cache: PromptCacheKind,
    pub streaming: bool,

    pub modalities_in: HashSet<Modality>,
    pub modalities_out: HashSet<Modality>,

    pub pricing: Pricing,
}

/// Validation error for `ProviderProfile::validate`.
#[derive(Debug, thiserror::Error)]
pub enum ProviderProfileError {
    #[error("modalities_in must be non-empty")]
    EmptyModalitiesIn,
    #[error("modalities_out must be non-empty")]
    EmptyModalitiesOut,
    #[error("supports_structured_output = ToolUseEmulation requires supports_tool_use = Supported")]
    ToolUseEmulationNeedsToolUse,
    #[error("supports_parallel_tool_calls = true requires supports_tool_use = Supported")]
    ParallelToolCallsNeedsToolUse,
}

impl ProviderProfile {
    /// Checks that the profile is consistent: it has input and output modalities, and tool-use
    /// emulation or parallel tool calls come with tool use.
    ///
    /// # Errors
    ///
    /// Returns the first inconsistency found.
    pub fn validate(&self) -> Result<(), ProviderProfileError> {
        if self.modalities_in.is_empty() {
            return Err(ProviderProfileError::EmptyModalitiesIn);
        }
        if self.modalities_out.is_empty() {
            return Err(ProviderProfileError::EmptyModalitiesOut);
        }
        if self.supports_structured_output == Some(StructuredOutputMode::ToolUseEmulation)
            && self.supports_tool_use != Support::Supported
        {
            return Err(ProviderProfileError::ToolUseEmulationNeedsToolUse);
        }
        if self.supports_parallel_tool_calls && self.supports_tool_use != Support::Supported {
            return Err(ProviderProfileError::ParallelToolCallsNeedsToolUse);
        }
        Ok(())
    }

    /// Returns whether the provider holds output to the request's schema. JSON mode does not: it
    /// promises JSON of any shape. Tool-use emulation holds it as far as the profile knows the
    /// provider has tool use.
    pub fn schema_enforcement(&self) -> Support {
        match self.supports_structured_output {
            Some(StructuredOutputMode::None | StructuredOutputMode::JsonObjectMode) => {
                Support::Unsupported
            }
            Some(StructuredOutputMode::ToolUseEmulation) => self.supports_tool_use,
            Some(StructuredOutputMode::StrictSchema) => Support::Supported,
            None => Support::Unknown,
        }
    }

    /// Returns a profile reached through `interface` that claims nothing: no ceilings, no
    /// features, no streaming, no prompt cache, text in and out, no price. A provider's profile
    /// starts from it and names only what the provider has.
    pub fn nothing_claimed(interface: InterfaceKind) -> Self {
        Self {
            interface,
            gen_ai_provider_name: None,
            max_context_tokens: None,
            max_output_tokens: None,
            supports_tool_use: Support::Unsupported,
            supports_parallel_tool_calls: false,
            supports_structured_output: Some(StructuredOutputMode::None),
            supports_vision: Support::Unsupported,
            supports_thinking: Support::Unsupported,
            supports_chat_template_thinking: false,
            supports_cancel: false,
            prompt_cache: PromptCacheKind::None,
            streaming: false,
            modalities_in: HashSet::from([Modality::Text]),
            modalities_out: HashSet::from([Modality::Text]),
            pricing: Pricing::default(),
        }
    }

    /// Returns a text-only profile with no extras: 32K context, 4K output, streaming.
    pub fn text_only_baseline(pricing: Pricing) -> Self {
        Self {
            max_context_tokens: Some(32_000),
            max_output_tokens: Some(4_096),
            streaming: true,
            pricing,
            ..Self::nothing_claimed(InterfaceKind::Http)
        }
    }

    /// Returns the profile of a server tars drives over its own HTTP adapter and has no data on,
    /// such as an `openai_compat` pointed at a local server: tool use, structured output,
    /// thinking and vision are unknown and no ceiling is enforced, so every request is sent and
    /// the server answers for itself. The adapter streams, so `streaming` holds.
    pub fn undescribed_http() -> Self {
        Self {
            supports_tool_use: Support::Unknown,
            supports_structured_output: None,
            supports_vision: Support::Unknown,
            supports_thinking: Support::Unknown,
            streaming: true,
            ..Self::nothing_claimed(InterfaceKind::Http)
        }
    }
}

/// What tars knows of one capability of a provider. `Unknown` is a profile with no data on it
/// (a local server whose model the data file does not describe); a request is refused only for
/// a capability that is `Unsupported`.
///
/// It is written `true`, `false` and `null`, so a profile saved with a plain boolean still reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Option<bool>", into = "Option<bool>")]
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
}

impl From<bool> for Support {
    fn from(supported: bool) -> Self {
        if supported {
            Self::Supported
        } else {
            Self::Unsupported
        }
    }
}

impl From<Option<bool>> for Support {
    fn from(known: Option<bool>) -> Self {
        known.map_or(Self::Unknown, Self::from)
    }
}

impl From<Support> for Option<bool> {
    fn from(s: Support) -> Self {
        match s {
            Support::Supported => Some(true),
            Support::Unsupported => Some(false),
            Support::Unknown => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutputMode {
    None,
    /// `{"type":"json_object"}`: JSON output with no schema enforced. `data/provider.toml` may
    /// spell it `json_object`, as the wire does.
    #[serde(alias = "json_object")]
    JsonObjectMode,
    /// The schema is enforced while decoding (OpenAI strict, Gemini responseSchema).
    StrictSchema,
    /// A forced `tool_choice` stands in for strict output, as with Anthropic.
    ToolUseEmulation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheKind {
    None,
    /// The provider caches prefixes of at least `min_tokens` on its own (OpenAI: 1024).
    ImplicitPrefix {
        min_tokens: u32,
    },
    /// Inline `cache_control` markers (Anthropic).
    ExplicitMarker,
    /// Out-of-band `cachedContent` API (Gemini).
    ExplicitObject,
    /// The CLI binary tars runs keeps its own cache.
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
        let caps = ProviderProfile::text_only_baseline(Pricing::default());
        assert!(caps.modalities_in.contains(&Modality::Text));
        assert!(caps.modalities_out.contains(&Modality::Text));
        assert_eq!(caps.modalities_in.len(), 1);
    }

    #[test]
    fn validate_rejects_empty_modalities() {
        let mut caps = ProviderProfile::text_only_baseline(Pricing::default());
        caps.modalities_in.clear();
        assert!(matches!(
            caps.validate(),
            Err(ProviderProfileError::EmptyModalitiesIn)
        ));

        let mut caps = ProviderProfile::text_only_baseline(Pricing::default());
        caps.modalities_out.clear();
        assert!(matches!(
            caps.validate(),
            Err(ProviderProfileError::EmptyModalitiesOut)
        ));
    }

    #[test]
    fn validate_rejects_tool_use_emulation_without_tool_use() {
        let mut caps = ProviderProfile::text_only_baseline(Pricing::default());
        caps.supports_structured_output = Some(StructuredOutputMode::ToolUseEmulation);
        assert!(matches!(
            caps.validate(),
            Err(ProviderProfileError::ToolUseEmulationNeedsToolUse)
        ));

        caps.supports_tool_use = Support::Supported;
        assert!(caps.validate().is_ok());
    }

    #[test]
    fn tool_use_emulation_enforces_output_schema_as_far_as_tool_use_is_known() {
        let mut caps = ProviderProfile::text_only_baseline(Pricing::default());
        caps.supports_structured_output = Some(StructuredOutputMode::ToolUseEmulation);
        assert_eq!(caps.schema_enforcement(), Support::Unsupported);
        caps.supports_tool_use = Support::Unknown;
        assert_eq!(caps.schema_enforcement(), Support::Unknown);
        caps.supports_tool_use = Support::Supported;
        assert_eq!(caps.schema_enforcement(), Support::Supported);
    }

    #[test]
    fn schema_enforcement_is_unknown_only_when_the_profile_has_no_data() {
        let caps = |mode| ProviderProfile {
            supports_structured_output: mode,
            ..ProviderProfile::nothing_claimed(InterfaceKind::Http)
        };
        assert_eq!(caps(None).schema_enforcement(), Support::Unknown);
        assert_eq!(
            caps(Some(StructuredOutputMode::StrictSchema)).schema_enforcement(),
            Support::Supported
        );
        for mode in [
            StructuredOutputMode::None,
            StructuredOutputMode::JsonObjectMode,
        ] {
            assert_eq!(caps(Some(mode)).schema_enforcement(), Support::Unsupported);
        }
    }

    #[test]
    fn unknown_structured_output_is_written_null_and_no_mechanism_is_named_unknown() {
        let p = ProviderProfile::undescribed_http();
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["supports_structured_output"], serde_json::Value::Null);
        assert!(serde_json::from_str::<StructuredOutputMode>(r#""unknown""#).is_err());
        assert_eq!(
            serde_json::from_str::<StructuredOutputMode>(r#""json_object""#).unwrap(),
            StructuredOutputMode::JsonObjectMode
        );
    }

    #[test]
    fn support_is_written_as_a_boolean_or_null() {
        for (s, wire) in [
            (Support::Supported, "true"),
            (Support::Unsupported, "false"),
            (Support::Unknown, "null"),
        ] {
            assert_eq!(serde_json::to_string(&s).unwrap(), wire);
            assert_eq!(serde_json::from_str::<Support>(wire).unwrap(), s);
        }
    }

    #[test]
    fn an_undescribed_server_claims_no_feature_and_no_ceiling() {
        let p = ProviderProfile::undescribed_http();
        assert_eq!(p.supports_tool_use, Support::Unknown);
        assert_eq!(p.supports_structured_output, None);
        assert_eq!(p.supports_thinking, Support::Unknown);
        assert_eq!(p.supports_vision, Support::Unknown);
        assert_eq!(p.max_context_tokens, None);
        assert_eq!(p.max_output_tokens, None);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn a_profile_that_claims_nothing_is_consistent() {
        assert!(
            ProviderProfile::nothing_claimed(InterfaceKind::Api)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn baseline_validates_clean() {
        assert!(
            ProviderProfile::text_only_baseline(Pricing::default())
                .validate()
                .is_ok()
        );
    }
}
