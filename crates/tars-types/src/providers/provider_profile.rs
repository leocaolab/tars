//! Describes what a provider can do and how tars reaches it, for routing and middleware to
//! consult before sending a request.

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
    /// the provider documents none or because it is unknown. The output ceiling is not here: it
    /// is per model, the [`OutputLimit`](crate::OutputLimit) the service binds.
    pub max_context_tokens: Option<u32>,

    pub tool_use: ToolUse,
    /// The mechanism that holds output to a schema; `None` when the profile has no data on it,
    /// written `null` as an unknown [`Support`] is.
    pub supports_structured_output: Option<StructuredOutputMode>,
    pub supports_thinking: Support,
    /// Whether the server reads the chat template's thinking switch
    /// (`chat_template_kwargs.enable_thinking`), as vLLM, mlx_lm.server and llama-server do; a
    /// vendor API has no chat template. Absent in a profile written without it, it is `false`.
    #[serde(default)]
    pub supports_chat_template_thinking: bool,
    pub supports_cancel: bool,

    pub prompt_cache: PromptCacheKind,
    pub streaming: bool,

    pub modalities_in: Modalities,
    pub modalities_out: Modalities,

    pub pricing: Pricing,
}

impl ProviderProfile {
    /// Returns whether the provider holds output to the request's schema. JSON mode does not: it
    /// promises JSON of any shape. Tool-use emulation holds it as far as the profile knows the
    /// provider has tool use.
    pub fn schema_enforcement(&self) -> Support {
        match self.supports_structured_output {
            Some(StructuredOutputMode::None | StructuredOutputMode::JsonObjectMode) => {
                Support::Unsupported
            }
            Some(StructuredOutputMode::ToolUseEmulation) => self.tool_use.support(),
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
            tool_use: ToolUse::Unsupported,
            supports_structured_output: Some(StructuredOutputMode::None),
            supports_thinking: Support::Unsupported,
            supports_chat_template_thinking: false,
            supports_cancel: false,
            prompt_cache: PromptCacheKind::None,
            streaming: false,
            modalities_in: Modalities::text_only(),
            modalities_out: Modalities::text_only(),
            pricing: Pricing::default(),
        }
    }

    /// Returns a text-only profile with no extras: 32K context, streaming.
    pub fn text_only_baseline(pricing: Pricing) -> Self {
        Self {
            max_context_tokens: Some(32_000),
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
            tool_use: ToolUse::Unknown,
            supports_structured_output: None,
            supports_thinking: Support::Unknown,
            modalities_in: Modalities::text_and_unknown(),
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

/// Whether a provider takes tools; calling several in one turn is something only a provider
/// with tool use can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolUse {
    Unsupported,
    /// The profile has no data on it, and a request with tools is still sent.
    Unknown,
    Supported {
        /// Whether the model may call several tools in one turn.
        parallel: bool,
    },
}

impl ToolUse {
    /// Returns what the profile knows of tool use, parallel calls aside.
    pub fn support(self) -> Support {
        match self {
            Self::Unsupported => Support::Unsupported,
            Self::Unknown => Support::Unknown,
            Self::Supported { .. } => Support::Supported,
        }
    }

    /// Returns whether the model may call several tools in one turn.
    pub fn parallel(self) -> bool {
        matches!(self, Self::Supported { parallel: true })
    }
}

/// What a provider knows of each kind of content it takes in, or gives out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modalities {
    pub text: Support,
    pub image: Support,
    pub audio: Support,
    pub video: Support,
    pub pdf: Support,
}

impl Modalities {
    /// Returns text alone, every other modality unsupported.
    pub fn text_only() -> Self {
        Self {
            text: Support::Supported,
            image: Support::Unsupported,
            audio: Support::Unsupported,
            video: Support::Unsupported,
            pdf: Support::Unsupported,
        }
    }

    /// Returns text, and nothing known of any other modality: a profile with no data on the
    /// model claims none and refuses none.
    pub fn text_and_unknown() -> Self {
        Self {
            text: Support::Supported,
            image: Support::Unknown,
            audio: Support::Unknown,
            video: Support::Unknown,
            pdf: Support::Unknown,
        }
    }

    /// Returns `listed` as supported and every other modality as unsupported.
    pub fn listed(listed: impl IntoIterator<Item = Modality>) -> Self {
        listed.into_iter().fold(
            Self {
                text: Support::Unsupported,
                ..Self::text_only()
            },
            |m, one| m.with(one, Support::Supported),
        )
    }

    /// Returns what the profile knows of `m`.
    pub fn of(&self, m: Modality) -> Support {
        match m {
            Modality::Text => self.text,
            Modality::Image => self.image,
            Modality::Audio => self.audio,
            Modality::Video => self.video,
            Modality::Pdf => self.pdf,
        }
    }

    fn with(mut self, m: Modality, support: Support) -> Self {
        *match m {
            Modality::Text => &mut self.text,
            Modality::Image => &mut self.image,
            Modality::Audio => &mut self.audio,
            Modality::Video => &mut self.video,
            Modality::Pdf => &mut self.pdf,
        } = support;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
    Pdf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_baseline_has_text_modalities() {
        let caps = ProviderProfile::text_only_baseline(Pricing::default());
        assert_eq!(caps.modalities_in, Modalities::text_only());
        assert_eq!(caps.modalities_out, Modalities::text_only());
    }

    #[test]
    fn a_listed_modality_is_supported_and_the_rest_are_not() {
        let m = Modalities::listed([Modality::Text, Modality::Image]);
        assert_eq!(m.of(Modality::Image), Support::Supported);
        assert_eq!(m.of(Modality::Pdf), Support::Unsupported);
        assert_eq!(
            Modalities::text_and_unknown().of(Modality::Image),
            Support::Unknown
        );
    }

    #[test]
    fn parallel_tool_calls_are_only_where_tool_use_is() {
        assert!(ToolUse::Supported { parallel: true }.parallel());
        assert!(!ToolUse::Supported { parallel: false }.parallel());
        assert!(!ToolUse::Unknown.parallel());
        assert!(!ToolUse::Unsupported.parallel());
    }

    #[test]
    fn tool_use_emulation_enforces_output_schema_as_far_as_tool_use_is_known() {
        let mut caps = ProviderProfile::text_only_baseline(Pricing::default());
        caps.supports_structured_output = Some(StructuredOutputMode::ToolUseEmulation);
        assert_eq!(caps.schema_enforcement(), Support::Unsupported);
        caps.tool_use = ToolUse::Unknown;
        assert_eq!(caps.schema_enforcement(), Support::Unknown);
        caps.tool_use = ToolUse::Supported { parallel: false };
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
        assert_eq!(p.tool_use, ToolUse::Unknown);
        assert_eq!(p.supports_structured_output, None);
        assert_eq!(p.supports_thinking, Support::Unknown);
        assert_eq!(p.modalities_in.image, Support::Unknown);
        assert_eq!(p.max_context_tokens, None);
    }
}
