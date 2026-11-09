//! Defines the request every provider accepts, and checks a request or a caller's declared needs
//! against a provider's profile.

use serde::{Deserialize, Serialize};

use crate::non_empty::NonEmpty;
use crate::providers::cache::CacheDirective;
use crate::providers::model::ThinkingMode;
use crate::providers::output_limit::OutputLimit;
use crate::providers::provider_profile::{ProviderProfile, Support, ToolUse};
use crate::providers::schema::JsonSchema;
use crate::providers::tools::{ToolChoice, ToolSpec};

/// A chat request, independent of provider and model, so one request can go to any of them. The
/// model is not on it: the leaf `LlmService` binds `provider + model` and passes the model to
/// `LlmProvider::stream`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatRequest {
    /// The system prompt, sent as `system` to OpenAI and Anthropic and as `system_instruction` to
    /// Gemini.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,

    /// The messages in order. Alternating user and assistant turns is not enforced here; an
    /// adapter whose vendor requires it does that.
    pub messages: Vec<Message>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,

    #[serde(default)]
    pub tool_choice: ToolChoice,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<JsonSchema>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache_directives: Vec<CacheDirective>,

    /// How much the provider may think; `None` says nothing and leaves the provider's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingMode>,

    /// The OpenAI-compat `chat_template_kwargs.enable_thinking` flag (Qwen3, mlx_lm.server, vLLM
    /// with a Qwen-family template); `None` leaves the server's default. It is apart from
    /// [`ThinkingMode`] because only a server with a chat template reads it. A provider whose profile
    /// has no `supports_chat_template_thinking` is not sent it, and
    /// [`ChatRequest::compatibility_check`] answers [`Degradation::ChatTemplateThinkingNotSent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_chat_template_thinking: Option<bool>,
}

impl ChatRequest {
    /// Returns a request of one user message, every other field unset.
    pub fn user(prompt: impl Into<String>) -> Self {
        Self {
            system: None,
            messages: vec![Message::User {
                content: vec![ContentBlock::text(prompt)],
            }],
            tools: Vec::new(),
            tool_choice: ToolChoice::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: Vec::new(),
            seed: None,
            cache_directives: Vec::new(),
            thinking: None,
            enable_chat_template_thinking: None,
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Returns the UTF-8 bytes of the system prompt and every text block; images and tool calls
    /// are not counted.
    pub fn text_bytes(&self) -> usize {
        self.messages
            .iter()
            .flat_map(|m| m.content())
            .filter_map(ContentBlock::as_text)
            .map(str::len)
            .fold(
                self.system.as_deref().map_or(0, str::len),
                usize::saturating_add,
            )
    }

    /// Checks the request against a provider's profile and the bound model's output ceiling
    /// before it is sent: whether the provider cannot serve it, or serves it without some fields.
    ///
    /// Only what the profile knows the provider lacks makes it unservable: a capability it has no
    /// data on ([`Support::Unknown`]) and a ceiling nobody states are not findings, and the
    /// provider answers for itself.
    pub fn compatibility_check(
        &self,
        caps: &ProviderProfile,
        output_limit: &OutputLimit,
    ) -> CompatibilityCheck {
        let mut reasons = Vec::new();
        let mut unsent = Vec::new();

        // `tool_choice` without tools is left for the provider to refuse.
        if !self.tools.is_empty() && caps.tool_use == ToolUse::Unsupported {
            reasons.push(CompatibilityReason::ToolUseUnsupported {
                tool_count: self.tools.len(),
            });
        }

        if self.structured_output.is_some() && caps.schema_enforcement() == Support::Unsupported {
            unsent.push(Degradation::StructuredOutputNotEnforced);
        }

        if self.enable_chat_template_thinking.is_some() && !caps.supports_chat_template_thinking {
            unsent.push(Degradation::ChatTemplateThinkingNotSent);
        }
        if let Some(mode @ (ThinkingMode::Auto | ThinkingMode::Budget(_))) = self.thinking
            && caps.supports_thinking == Support::Unsupported
        {
            reasons.push(CompatibilityReason::ThinkingUnsupported { mode });
        }

        let has_image = self.messages.iter().any(|m| {
            m.content()
                .iter()
                .any(|c| matches!(c, ContentBlock::Image { .. }))
        });
        if has_image && caps.modalities_in.image == Support::Unsupported {
            reasons.push(CompatibilityReason::VisionUnsupported);
        }

        // The byte estimate mostly underestimates the token count, so only an obvious overflow is
        // flagged; a borderline request goes through and the provider refuses it with
        // ContextTooLong.
        let estimated_tokens =
            u32::try_from(crate::estimate_tokens(self.text_bytes())).unwrap_or(u32::MAX);
        if let Some(max_ctx) = caps.max_context_tokens
            && estimated_tokens > max_ctx
        {
            reasons.push(CompatibilityReason::ContextWindowExceeded {
                estimated_prompt_tokens: estimated_tokens,
                max_context_tokens: max_ctx,
            });
        }

        if let (Some(req_max), Some(max_out)) = (self.max_output_tokens, output_limit.tokens())
            && req_max > max_out
        {
            reasons.push(CompatibilityReason::MaxOutputTokensExceeded {
                requested: req_max,
                max: max_out,
            });
        }

        CompatibilityCheck::from_findings(reasons, unsent)
    }
}

/// Returns the byte length of the request's text, the system prompt included.
/// Whether one provider can serve a request, and what it leaves out. It says nothing of other
/// providers: a caller bound to this one provider refuses on `Incompatible`, a router tries the
/// next candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompatibilityCheck {
    /// This provider can serve the request as it is.
    Compatible,
    /// This provider serves the request without what `unsent` names; never empty.
    Degraded { unsent: NonEmpty<Degradation> },
    /// This provider cannot serve the request, for every one of `reasons`; never empty. What it
    /// would also have left out is not listed, since the request is not sent.
    Incompatible {
        reasons: NonEmpty<CompatibilityReason>,
    },
}

impl CompatibilityCheck {
    fn from_findings(reasons: Vec<CompatibilityReason>, unsent: Vec<Degradation>) -> Self {
        match (NonEmpty::from_vec(reasons), NonEmpty::from_vec(unsent)) {
            (Some(reasons), _) => Self::Incompatible { reasons },
            (None, Some(unsent)) => Self::Degraded { unsent },
            (None, None) => Self::Compatible,
        }
    }
}

/// A part of a [`ChatRequest`] a provider can serve the request without; the adapter leaves it
/// out, as the profile says, and the call goes ahead.
///
/// The serde names are a storage format: the call log stores them, so a variant's name is never
/// changed and a variant is never removed, or every file holding it stops decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Degradation {
    /// The request carries a `structured_output` schema the provider will not enforce. The
    /// adapter sends JSON mode or nothing, and the prompt has to elicit the shape.
    #[serde(rename = "structured_output")]
    StructuredOutputNotEnforced,
    /// The request sets `enable_chat_template_thinking` and the server has no chat template
    /// that reads it, so the switch is not sent and the model thinks as it would by default.
    #[serde(rename = "chat_template_thinking")]
    ChatTemplateThinkingNotSent,
}

impl Degradation {
    /// Returns the snake_case tag for telemetry and tars-py, the word serde writes.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::StructuredOutputNotEnforced => "structured_output",
            Self::ChatTemplateThinkingNotSent => "chat_template_thinking",
        }
    }
}

impl std::fmt::Display for Degradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::StructuredOutputNotEnforced => {
                "request carries a structured_output schema the provider does not enforce; the \
                 prompt has to elicit the shape"
            }
            Self::ChatTemplateThinkingNotSent => {
                "request sets enable_chat_template_thinking but the provider has no chat template \
                 that reads it; the switch is not sent"
            }
        })
    }
}

/// One way a provider's [`ProviderProfile`] cannot serve what a [`ChatRequest`] needs, for a
/// caller to branch on (drop the tools and retry, or fail on a context overflow).
///
/// The serde form is a storage format: the call log stores a refused call's reasons, so a
/// variant's `kind` tag and its field names are never changed and a variant is never removed, or
/// every file holding it stops decoding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
#[non_exhaustive]
pub enum CompatibilityReason {
    /// The request has tools and the provider has no tool use. `tool_count` is 0 when it comes
    /// from [`ProviderProfile::check_requirements`].
    #[serde(rename = "tool_use")]
    ToolUseUnsupported { tool_count: usize },
    /// The request asks for thinking and the provider has none.
    #[serde(rename = "thinking")]
    ThinkingUnsupported { mode: ThinkingMode },
    /// The request contains an image and the provider has no vision.
    #[serde(rename = "vision")]
    VisionUnsupported,
    /// The estimated prompt size exceeds the provider's `max_context_tokens`.
    #[serde(rename = "context_window")]
    ContextWindowExceeded {
        /// The prompt's byte length / 4, or the caller's declared estimate.
        estimated_prompt_tokens: u32,
        max_context_tokens: u32,
    },
    /// The requested `max_output_tokens` exceeds the provider's.
    #[serde(rename = "max_output_tokens")]
    MaxOutputTokensExceeded { requested: u32, max: u32 },
}

impl std::fmt::Display for CompatibilityReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolUseUnsupported { tool_count } => write!(
                f,
                "request has {tool_count} tool(s) but provider does not support tool_use"
            ),
            Self::ThinkingUnsupported { mode } => write!(
                f,
                "request has thinking={mode:?} but provider does not support thinking"
            ),
            Self::VisionUnsupported => write!(
                f,
                "request contains image content but provider does not support vision"
            ),
            Self::ContextWindowExceeded {
                estimated_prompt_tokens,
                max_context_tokens,
            } => write!(
                f,
                "estimated prompt size {estimated_prompt_tokens} tokens (bytes/4 heuristic) exceeds provider max_context_tokens={max_context_tokens}"
            ),
            Self::MaxOutputTokensExceeded { requested, max } => write!(
                f,
                "request max_output_tokens={requested} exceeds provider max={max}"
            ),
        }
    }
}

impl CompatibilityReason {
    /// Returns the reason's snake_case tag for telemetry and tars-py, the `kind` serde writes.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ToolUseUnsupported { .. } => "tool_use",
            Self::ThinkingUnsupported { .. } => "thinking",
            Self::VisionUnsupported => "vision",
            Self::ContextWindowExceeded { .. } => "context_window",
            Self::MaxOutputTokensExceeded { .. } => "max_output_tokens",
        }
    }
}

/// What a caller will need from a provider, declared at startup before any request exists, for
/// [`ProviderProfile::check_requirements`]. Every field defaults to "not needed".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderNeeds {
    pub requires_tools: bool,
    pub requires_vision: bool,
    pub requires_thinking: bool,
    pub requires_structured_output: bool,
    /// The largest prompt the caller will send, in tokens; 0 means not checked.
    pub estimated_max_prompt_tokens: u32,
    /// The largest `max_output_tokens` the caller will request; 0 means not checked.
    pub estimated_max_output_tokens: u32,
}

impl ProviderProfile {
    /// Checks the profile and the bound model's output ceiling against a caller's declared needs
    /// at startup, returning every unmet one in the same verdict
    /// [`ChatRequest::compatibility_check`] gives.
    pub fn check_requirements(
        &self,
        req: &ProviderNeeds,
        output_limit: &OutputLimit,
    ) -> CompatibilityCheck {
        let mut reasons = Vec::new();
        let mut unsent = Vec::new();

        if req.requires_tools && self.tool_use == ToolUse::Unsupported {
            reasons.push(CompatibilityReason::ToolUseUnsupported { tool_count: 0 });
        }
        if req.requires_vision && self.modalities_in.image == Support::Unsupported {
            reasons.push(CompatibilityReason::VisionUnsupported);
        }
        if req.requires_thinking && self.supports_thinking == Support::Unsupported {
            // The caller declared no mode; `Auto` stands for "any thinking".
            reasons.push(CompatibilityReason::ThinkingUnsupported {
                mode: ThinkingMode::Auto,
            });
        }
        if req.requires_structured_output && self.schema_enforcement() == Support::Unsupported {
            unsent.push(Degradation::StructuredOutputNotEnforced);
        }
        if let Some(max_ctx) = self.max_context_tokens
            && req.estimated_max_prompt_tokens > 0
            && req.estimated_max_prompt_tokens > max_ctx
        {
            reasons.push(CompatibilityReason::ContextWindowExceeded {
                estimated_prompt_tokens: req.estimated_max_prompt_tokens,
                max_context_tokens: max_ctx,
            });
        }
        if let Some(max_out) = output_limit.tokens()
            && req.estimated_max_output_tokens > 0
            && req.estimated_max_output_tokens > max_out
        {
            reasons.push(CompatibilityReason::MaxOutputTokensExceeded {
                requested: req.estimated_max_output_tokens,
                max: max_out,
            });
        }

        CompatibilityCheck::from_findings(reasons, unsent)
    }
}

/// One message of a conversation, by role as OpenAI and Anthropic model it. A tool call's result
/// goes back as a `Tool` message naming the call's id.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: Vec<ContentBlock>,
    },
    Assistant {
        // OpenAI omits `content` on an assistant message that only calls tools.
        #[serde(default)]
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<crate::providers::tools::ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
    },
    /// A system message kept inline, as in a replayed history; the system prompt usually goes in
    /// `request.system`.
    System {
        content: Vec<ContentBlock>,
    },
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![ContentBlock::text(text)],
            tool_calls: Vec::new(),
        }
    }

    pub fn content(&self) -> &[ContentBlock] {
        match self {
            Self::User { content }
            | Self::Assistant { content, .. }
            | Self::Tool { content, .. }
            | Self::System { content } => content,
        }
    }
}

/// One block of a message's content, serialized as OpenAI and Anthropic write it:
/// `{"type": "text", "text": "..."}` / `{"type": "image", ...}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { mime: String, data: ImageData },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        if let Self::Text { text } = self {
            Some(text)
        } else {
            None
        }
    }
}

/// An image as a URL or as base64 text; raw bytes are never carried. The serde form is a storage
/// format, since transcripts and the call log store messages: `{"url":"..."}` / `{"base64":"..."}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageData {
    Url(String),
    Base64(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::provider_profile::StructuredOutputMode;

    #[test]
    fn user_builder_creates_minimal_request() {
        let r = ChatRequest::user("hi");
        assert_eq!(r.messages.len(), 1);
        assert!(matches!(r.messages[0], Message::User { .. }));
    }

    #[test]
    fn message_serializes_with_role_tag() {
        let m = Message::user_text("hi");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "user");
        assert!(v["content"].is_array());
    }

    #[test]
    fn every_image_is_written_under_its_kind_and_reads_back() {
        let stored = [
            (
                ImageData::Url("https://x/y.png".into()),
                r#"{"url":"https://x/y.png"}"#,
            ),
            (ImageData::Base64("AA==".into()), r#"{"base64":"AA=="}"#),
        ];
        for (image, json) in stored {
            assert_eq!(serde_json::to_string(&image).unwrap(), json, "{image:?}");
            assert_eq!(serde_json::from_str::<ImageData>(json).unwrap(), image);
        }
    }

    // ── compatibility_check tests ───────────────────────────────────────────

    use crate::providers::provider_profile::{
        Modalities, Modality, PromptCacheKind, ProviderProfile,
    };
    use crate::providers::schema::JsonSchema;
    use crate::providers::usage::Pricing;

    const MINIMAL_LIMIT: OutputLimit = OutputLimit::Configured { tokens: 4096 };
    const FULL_LIMIT: OutputLimit = OutputLimit::Configured { tokens: 8192 };
    const ZERO_LIMIT: OutputLimit = OutputLimit::Configured { tokens: 0 };

    fn caps_minimal() -> ProviderProfile {
        ProviderProfile {
            max_context_tokens: Some(32_768),
            supports_cancel: true,
            streaming: true,
            ..ProviderProfile::nothing_claimed(crate::InterfaceKind::Http)
        }
    }

    fn caps_full() -> ProviderProfile {
        let both = Modalities::listed([Modality::Text, Modality::Image]);
        ProviderProfile {
            max_context_tokens: Some(200_000),
            tool_use: ToolUse::Supported { parallel: true },
            supports_structured_output: Some(StructuredOutputMode::StrictSchema),
            supports_thinking: Support::Supported,
            supports_chat_template_thinking: true,
            supports_cancel: true,
            prompt_cache: PromptCacheKind::ImplicitPrefix { min_tokens: 1024 },
            streaming: true,
            modalities_in: both,
            modalities_out: both,
            ..ProviderProfile::nothing_claimed(crate::InterfaceKind::Http)
        }
    }

    #[test]
    fn a_chat_template_switch_the_server_cannot_read_degrades_the_call_and_does_not_refuse_it() {
        let mut req = ChatRequest::user("hi");
        req.enable_chat_template_thinking = Some(false);
        assert_eq!(
            req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT),
            CompatibilityCheck::Degraded {
                unsent: NonEmpty::new(Degradation::ChatTemplateThinkingNotSent, [])
            }
        );
        assert_eq!(
            req.compatibility_check(&caps_full(), &FULL_LIMIT),
            CompatibilityCheck::Compatible
        );
        req.enable_chat_template_thinking = None;
        assert_eq!(
            req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT),
            CompatibilityCheck::Compatible
        );
    }

    #[test]
    fn a_schema_json_mode_does_not_enforce_degrades_the_call() {
        let mut req = ChatRequest::user("hi");
        req.structured_output = Some(crate::JsonSchema::strict(
            "R",
            crate::json_object! {"type": "object"},
        ));
        let mut json_mode = caps_full();
        json_mode.supports_structured_output = Some(StructuredOutputMode::JsonObjectMode);
        assert_eq!(
            req.compatibility_check(&json_mode, &FULL_LIMIT),
            CompatibilityCheck::Degraded {
                unsent: NonEmpty::new(Degradation::StructuredOutputNotEnforced, [])
            }
        );
        assert_eq!(
            req.compatibility_check(&caps_full(), &FULL_LIMIT),
            CompatibilityCheck::Compatible
        );
    }

    #[test]
    fn a_request_that_is_both_degraded_and_unservable_is_incompatible() {
        let mut req = ChatRequest::user("hi");
        req.enable_chat_template_thinking = Some(true);
        req.thinking = Some(ThinkingMode::Auto);
        assert_eq!(
            req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT),
            CompatibilityCheck::Incompatible {
                reasons: NonEmpty::new(
                    CompatibilityReason::ThinkingUnsupported {
                        mode: ThinkingMode::Auto
                    },
                    []
                )
            }
        );
    }

    #[test]
    fn compat_text_only_request_passes_minimal_provider() {
        let req = ChatRequest::user("hi");
        assert!(matches!(
            req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn compat_tools_blocked_by_no_tool_support() {
        use crate::providers::tools::ToolSpec;
        let mut req = ChatRequest::user("hi");
        req.tools.push(ToolSpec {
            name: "x".into(),
            description: "x".into(),
            input_schema: JsonSchema::loose(crate::json_object! {"type":"object"}),
        });
        match req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT) {
            CompatibilityCheck::Incompatible { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(matches!(
                    reasons[0],
                    CompatibilityReason::ToolUseUnsupported { tool_count: 1 }
                ));
                assert_eq!(reasons[0].kind(), "tool_use");
            }
            _ => panic!("expected Incompatible"),
        }
    }

    fn every_capability_and_a_long_prompt() -> ChatRequest {
        use crate::providers::tools::ToolSpec;
        let mut req = ChatRequest::user("x".repeat(150_000));
        req.tools.push(ToolSpec {
            name: "x".into(),
            description: "x".into(),
            input_schema: JsonSchema::loose(crate::json_object! {"type": "object"}),
        });
        req.thinking = Some(ThinkingMode::Auto);
        req.structured_output = Some(JsonSchema::strict(
            "R",
            crate::json_object! {"type": "object"},
        ));
        req.messages.push(Message::User {
            content: vec![ContentBlock::Image {
                mime: "image/png".into(),
                data: ImageData::Url("https://x/y.png".into()),
            }],
        });
        req
    }

    #[test]
    fn a_provider_with_no_data_on_a_capability_is_sent_the_request() {
        assert_eq!(
            every_capability_and_a_long_prompt()
                .compatibility_check(&ProviderProfile::undescribed_http(), &OutputLimit::Unknown),
            CompatibilityCheck::Compatible
        );
    }

    #[test]
    fn a_provider_known_to_lack_each_capability_is_refused_for_each() {
        let mut caps = ProviderProfile::undescribed_http();
        caps.tool_use = ToolUse::Unsupported;
        caps.supports_thinking = Support::Unsupported;
        caps.modalities_in.image = Support::Unsupported;
        caps.max_context_tokens = Some(32_000);
        let CompatibilityCheck::Incompatible { reasons } =
            every_capability_and_a_long_prompt().compatibility_check(&caps, &OutputLimit::Unknown)
        else {
            panic!("expected Incompatible");
        };
        let kinds: Vec<_> = reasons.iter().map(CompatibilityReason::kind).collect();
        assert_eq!(kinds, ["tool_use", "thinking", "vision", "context_window"]);
    }

    #[test]
    fn declared_needs_a_provider_has_no_data_on_pass_at_startup() {
        let needs = ProviderNeeds {
            requires_tools: true,
            requires_vision: true,
            requires_thinking: true,
            requires_structured_output: true,
            estimated_max_prompt_tokens: 1_000_000,
            estimated_max_output_tokens: 1_000_000,
        };
        assert_eq!(
            ProviderProfile::undescribed_http().check_requirements(&needs, &OutputLimit::Unknown),
            CompatibilityCheck::Compatible
        );
    }

    #[test]
    fn compat_tools_pass_when_provider_supports() {
        use crate::providers::tools::ToolSpec;
        let mut req = ChatRequest::user("hi");
        req.tools.push(ToolSpec {
            name: "x".into(),
            description: "x".into(),
            input_schema: JsonSchema::loose(crate::json_object! {"type":"object"}),
        });
        assert!(matches!(
            req.compatibility_check(&caps_full(), &FULL_LIMIT),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn compat_thinking_auto_blocked_when_not_supported() {
        let mut req = ChatRequest::user("hi");
        req.thinking = Some(ThinkingMode::Auto);
        match req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT) {
            CompatibilityCheck::Incompatible { reasons } => {
                assert!(reasons.iter().any(|r| matches!(
                    r,
                    CompatibilityReason::ThinkingUnsupported {
                        mode: ThinkingMode::Auto
                    }
                )));
            }
            _ => panic!("expected Incompatible"),
        }
    }

    #[test]
    fn compat_thinking_off_passes_anywhere() {
        let mut req = ChatRequest::user("hi");
        req.thinking = Some(ThinkingMode::Off);
        assert!(matches!(req.thinking, Some(ThinkingMode::Off)));
        assert!(matches!(
            req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn a_schema_the_provider_does_not_enforce_degrades_the_call_and_does_not_refuse_it() {
        let mut req = ChatRequest::user("hi");
        req.structured_output = Some(JsonSchema::loose(crate::json_object! {"type":"object"}));
        assert_eq!(
            req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT),
            CompatibilityCheck::Degraded {
                unsent: NonEmpty::new(Degradation::StructuredOutputNotEnforced, [])
            }
        );
    }

    #[test]
    fn compat_image_content_blocked_by_no_vision() {
        let req = ChatRequest {
            system: None,
            messages: vec![Message::User {
                content: vec![ContentBlock::Image {
                    mime: "image/png".into(),
                    data: ImageData::Url("https://x/y.png".into()),
                }],
            }],
            tools: Vec::new(),
            tool_choice: ToolChoice::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: Vec::new(),
            seed: None,
            cache_directives: Vec::new(),
            thinking: None,
            enable_chat_template_thinking: None,
        };
        match req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT) {
            CompatibilityCheck::Incompatible { reasons } => {
                assert!(
                    reasons
                        .iter()
                        .any(|r| matches!(r, CompatibilityReason::VisionUnsupported))
                );
            }
            _ => panic!("expected Incompatible"),
        }
    }

    #[test]
    fn compat_aggregates_multiple_reasons() {
        // Tools + thinking + structured + vision all rejected at once.
        use crate::providers::tools::ToolSpec;
        let mut req = ChatRequest {
            system: None,
            messages: vec![Message::User {
                content: vec![ContentBlock::Image {
                    mime: "image/png".into(),
                    data: ImageData::Url("https://x".into()),
                }],
            }],
            tools: Vec::new(),
            tool_choice: ToolChoice::default(),
            structured_output: Some(JsonSchema::loose(crate::json_object! {})),
            max_output_tokens: None,
            temperature: None,
            stop_sequences: Vec::new(),
            seed: None,
            cache_directives: Vec::new(),
            thinking: Some(ThinkingMode::Auto),
            enable_chat_template_thinking: None,
        };
        req.tools.push(ToolSpec {
            name: "x".into(),
            description: "x".into(),
            input_schema: JsonSchema::loose(crate::json_object! {}),
        });
        match req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT) {
            CompatibilityCheck::Incompatible { reasons } => {
                assert_eq!(
                    reasons.len(),
                    3,
                    "expected all 3 reasons collected, got: {reasons:?}"
                );
                // verify each kind shows up exactly once
                let kinds: std::collections::HashSet<_> =
                    reasons.iter().map(|r| r.kind()).collect();
                assert_eq!(
                    kinds,
                    ["tool_use", "thinking", "vision"].into_iter().collect()
                );
            }
            _ => panic!("expected Incompatible"),
        }
    }

    // ── Context window + max_output checks ───────────────────────────────

    #[test]
    fn compat_context_window_exceeded_flagged() {
        // Build a request whose prompt clearly exceeds 32k tokens
        // (chars/4 estimate). 32k tokens × 4 chars/tok ≈ 128k chars;
        // pad to 200k chars to be obviously over.
        let mut req = ChatRequest::user("x".repeat(200_000));
        req.system = None;
        match req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT) {
            CompatibilityCheck::Incompatible { reasons } => {
                let r = reasons
                    .iter()
                    .find(|r| matches!(r, CompatibilityReason::ContextWindowExceeded { .. }))
                    .expect("expected ContextWindowExceeded");
                if let CompatibilityReason::ContextWindowExceeded {
                    estimated_prompt_tokens,
                    max_context_tokens,
                } = r
                {
                    assert!(*estimated_prompt_tokens > 32_768);
                    assert_eq!(*max_context_tokens, 32_768);
                }
            }
            _ => panic!("expected Incompatible due to context overflow"),
        }
    }

    #[test]
    fn compat_max_output_tokens_exceeded_flagged() {
        let mut req = ChatRequest::user("hi");
        req.max_output_tokens = Some(8192); // MINIMAL_LIMIT is 4096
        match req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT) {
            CompatibilityCheck::Incompatible { reasons } => {
                assert!(reasons.iter().any(|r| matches!(
                    r,
                    CompatibilityReason::MaxOutputTokensExceeded {
                        requested: 8192,
                        max: 4096
                    }
                )));
            }
            _ => panic!("expected Incompatible"),
        }
    }

    #[test]
    fn compat_max_output_within_limit_passes() {
        let mut req = ChatRequest::user("hi");
        req.max_output_tokens = Some(4096); // exactly at the cap
        assert!(matches!(
            req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn the_output_ceiling_checked_is_the_bound_models_limit() {
        let mut req = ChatRequest::user("hi");
        req.max_output_tokens = Some(4097);
        assert!(matches!(
            req.compatibility_check(&caps_minimal(), &MINIMAL_LIMIT),
            CompatibilityCheck::Incompatible { .. }
        ));
        assert_eq!(
            req.compatibility_check(&caps_minimal(), &OutputLimit::Unknown),
            CompatibilityCheck::Compatible
        );
    }

    // ── Boundary cases ─────────────────────────────────────────────

    /// Returns the most restricted profile: no features, zero context and output.
    fn caps_zero() -> ProviderProfile {
        let mut c = ProviderProfile::text_only_baseline(Pricing::default());
        c.max_context_tokens = Some(0);
        c.tool_use = ToolUse::Unsupported;
        c.supports_structured_output = Some(StructuredOutputMode::None);
        c.modalities_in.image = Support::Unsupported;
        c.supports_thinking = Support::Unsupported;
        c.streaming = false;
        c
    }

    #[test]
    fn compat_baseline_capabilities_text_only_request_passes() {
        let caps = ProviderProfile::text_only_baseline(Pricing::default());
        let req = ChatRequest::user("hi");
        match req.compatibility_check(&caps, &MINIMAL_LIMIT) {
            CompatibilityCheck::Compatible => {}
            other => panic!("expected Compatible, got: {other:?}"),
        }
    }

    #[test]
    fn compat_zero_capability_provider_with_full_feature_request_aggregates_all() {
        use crate::providers::tools::ToolSpec;
        let zero_caps = caps_zero(); // everything off, max_*_tokens = 0
        let req = ChatRequest {
            system: Some("x".repeat(200_000)), // overflow context window
            messages: vec![Message::User {
                content: vec![ContentBlock::Image {
                    mime: "image/png".into(),
                    data: ImageData::Url("https://x".into()),
                }],
            }],
            tools: vec![ToolSpec {
                name: "x".into(),
                description: "x".into(),
                input_schema: JsonSchema::loose(crate::json_object! {}),
            }],
            tool_choice: ToolChoice::default(),
            structured_output: Some(JsonSchema::loose(crate::json_object! {})),
            max_output_tokens: Some(99_999), // exceeds ZERO_LIMIT
            temperature: None,
            stop_sequences: Vec::new(),
            seed: None,
            cache_directives: Vec::new(),
            thinking: Some(ThinkingMode::Auto),
            enable_chat_template_thinking: None,
        };
        match req.compatibility_check(&zero_caps, &ZERO_LIMIT) {
            CompatibilityCheck::Incompatible { reasons } => {
                let kinds: std::collections::HashSet<_> =
                    reasons.iter().map(|r| r.kind()).collect();
                assert!(kinds.contains("tool_use"));
                assert!(kinds.contains("thinking"));
                assert!(kinds.contains("vision"));
                assert!(kinds.contains("context_window"));
                assert!(kinds.contains("max_output_tokens"));
                assert_eq!(kinds.len(), 5, "expected all 5 axes flagged: {kinds:?}");
            }
            _ => panic!("expected Incompatible with all 5 reasons"),
        }
    }

    // ── check_requirements (config-time API) ─────────────────────

    #[test]
    fn requirements_default_is_empty_and_passes_minimal_caps() {
        let reqs = ProviderNeeds::default();
        assert!(matches!(
            caps_minimal().check_requirements(&reqs, &MINIMAL_LIMIT),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn requirements_tools_blocked_by_no_tool_caps() {
        let reqs = ProviderNeeds {
            requires_tools: true,
            ..Default::default()
        };
        match caps_minimal().check_requirements(&reqs, &MINIMAL_LIMIT) {
            CompatibilityCheck::Incompatible { reasons } => {
                assert!(
                    reasons
                        .iter()
                        .any(|r| matches!(r, CompatibilityReason::ToolUseUnsupported { .. }))
                );
            }
            _ => panic!("expected Incompatible"),
        }
    }

    #[test]
    fn requirements_full_feature_set_aggregates_all_reasons() {
        let reqs = ProviderNeeds {
            requires_tools: true,
            requires_vision: true,
            requires_thinking: true,
            requires_structured_output: true,
            estimated_max_prompt_tokens: 100_000, // > caps_minimal's 32k
            estimated_max_output_tokens: 8_000,   // > caps_minimal's 4096
        };
        match caps_minimal().check_requirements(&reqs, &MINIMAL_LIMIT) {
            CompatibilityCheck::Incompatible { reasons } => {
                let kinds: std::collections::HashSet<_> =
                    reasons.iter().map(|r| r.kind()).collect();
                assert_eq!(kinds.len(), 5, "expected all 5 axes flagged: {kinds:?}");
            }
            _ => panic!("expected Incompatible"),
        }
    }

    #[test]
    fn requirements_zero_estimated_tokens_means_no_check() {
        let reqs = ProviderNeeds {
            estimated_max_prompt_tokens: 0,
            estimated_max_output_tokens: 0,
            ..Default::default()
        };
        let mut caps = caps_minimal();
        caps.max_context_tokens = Some(0);
        assert!(matches!(
            caps.check_requirements(&reqs, &ZERO_LIMIT),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn requirements_full_caps_passes_demanding_requirements() {
        let reqs = ProviderNeeds {
            requires_tools: true,
            requires_vision: true,
            requires_thinking: true,
            requires_structured_output: true,
            estimated_max_prompt_tokens: 100_000, // < caps_full's 200k
            estimated_max_output_tokens: 4_000,   // < caps_full's 8k
        };
        assert!(matches!(
            caps_full().check_requirements(&reqs, &FULL_LIMIT),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn compat_reason_display_renders_useful_messages() {
        let r = CompatibilityReason::ToolUseUnsupported { tool_count: 3 };
        assert!(r.to_string().contains("3 tool(s)"));
        assert!(r.to_string().contains("tool_use"));

        let r = CompatibilityReason::ContextWindowExceeded {
            estimated_prompt_tokens: 50_000,
            max_context_tokens: 32_768,
        };
        let s = r.to_string();
        assert!(s.contains("50000"));
        assert!(s.contains("32768"));
        assert!(s.contains("bytes/4"));
    }

    #[test]
    fn every_compatibility_reason_is_stored_under_its_kind_and_reads_back() {
        let stored = [
            (
                CompatibilityReason::ToolUseUnsupported { tool_count: 2 },
                r#"{"kind":"tool_use","tool_count":2}"#,
            ),
            (
                CompatibilityReason::ThinkingUnsupported {
                    mode: ThinkingMode::Auto,
                },
                r#"{"kind":"thinking","mode":{"mode":"auto"}}"#,
            ),
            (
                CompatibilityReason::ThinkingUnsupported {
                    mode: ThinkingMode::Budget(2048),
                },
                r#"{"kind":"thinking","mode":{"mode":"budget","tokens":2048}}"#,
            ),
            (
                CompatibilityReason::VisionUnsupported,
                r#"{"kind":"vision"}"#,
            ),
            (
                CompatibilityReason::ContextWindowExceeded {
                    estimated_prompt_tokens: 9,
                    max_context_tokens: 8,
                },
                r#"{"kind":"context_window","estimated_prompt_tokens":9,"max_context_tokens":8}"#,
            ),
            (
                CompatibilityReason::MaxOutputTokensExceeded {
                    requested: 5,
                    max: 4,
                },
                r#"{"kind":"max_output_tokens","requested":5,"max":4}"#,
            ),
        ];
        for (reason, json) in stored {
            assert_eq!(serde_json::to_string(&reason).unwrap(), json);
            assert!(json.contains(&format!(r#""kind":"{}""#, reason.kind())));
            assert_eq!(
                serde_json::from_str::<CompatibilityReason>(json).unwrap(),
                reason
            );
        }
    }
}
