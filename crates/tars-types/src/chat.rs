//! Canonical request shape — what every Provider accepts.

use serde::{Deserialize, Serialize};

use crate::cache::CacheDirective;
use crate::capabilities::{Capabilities, StructuredOutputMode};
use crate::model::{ModelHint, ThinkingMode};
use crate::schema::JsonSchema;
use crate::tools::{ToolChoice, ToolSpec};

/// A complete chat request. Provider-agnostic.
///
/// All fields except `model` and `messages` are optional / defaulted so
/// callers pay only for what they use.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: ModelHint,

    /// Hardcoded model behavior. Goes into `system` for OpenAI/Anthropic,
    /// `system_instruction` for Gemini.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,

    /// Chronological message list. Strictly alternating user/assistant
    /// is *not* enforced here (Provider adapter is responsible) but is
    /// the recommended layout.
    pub messages: Vec<Message>,

    /// Tool definitions made available to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,

    /// How aggressively the model can/must call tools.
    #[serde(default)]
    pub tool_choice: ToolChoice,

    /// Force the model to emit JSON matching this schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<JsonSchema>,

    /// Per-provider sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,

    /// Cache directives — see [`CacheDirective`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache_directives: Vec<CacheDirective>,

    /// Thinking / reasoning mode.
    #[serde(default, skip_serializing_if = "ThinkingMode::is_off")]
    pub thinking: ThinkingMode,

    /// Per-request override for the OpenAI-compat `chat_template_kwargs.
    /// enable_thinking` boolean (Qwen3 / mlx_lm.server / vLLM with a
    /// Qwen-family chat template). `None` = don't send the field, server
    /// uses its default. Distinct from [`ThinkingMode`] because that
    /// enum's `Off` doubles as "caller didn't specify"; this field needs
    /// to distinguish "explicitly off" from "no preference" so we don't
    /// silently force `enable_thinking=false` on every call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_chat_template_thinking: Option<bool>,
}

impl ChatRequest {
    /// Minimal builder for the common single-user-turn case.
    pub fn user(model: ModelHint, prompt: impl Into<String>) -> Self {
        Self {
            model,
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
            thinking: ThinkingMode::default(),
            enable_chat_template_thinking: None,
        }
    }

    /// Set system prompt (chainable).
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Check whether a provider with the given [`Capabilities`] can serve
    /// this request. Used by routing's pre-flight check (B-31) to skip
    /// fallback candidates that can't honour the request's feature
    /// requirements (tools / vision / thinking / structured output /
    /// context window / max output) — avoids wasting a wire round-trip
    /// on a candidate that would silently drop features or 400 at the
    /// provider.
    ///
    /// Returns [`CompatibilityCheck::Incompatible`] with a typed list
    /// of [`CompatibilityReason`]s when the request's feature set
    /// isn't covered by `caps`. Caller (routing layer / Python
    /// pre-check) can `match` on each reason to decide:
    ///
    /// - downgrade gracefully (e.g. drop tools, retry without)
    /// - swap providers (e.g. switch to a vision-capable model)
    /// - reject hard (e.g. context overflow can't be fixed by routing)
    ///
    /// **Conservative philosophy**: when in doubt (e.g., a feature
    /// is set but `caps` doesn't explicitly forbid it), return
    /// [`Compatible`]. False-positive incompatibility would cause
    /// the routing layer to skip a candidate that *would* have
    /// worked, which is worse than letting the provider return its
    /// own permanent error.
    pub fn compatibility_check(&self, caps: &Capabilities) -> CompatibilityCheck {
        let mut reasons = Vec::new();

        // Tools — non-empty tool list requires tool_use support.
        // We deliberately don't check `tool_choice` independently:
        // callers who set tool_choice without tools are misusing the
        // API and the provider will tell them so.
        if !self.tools.is_empty() && !caps.supports_tool_use {
            reasons.push(CompatibilityReason::ToolUseUnsupported {
                tool_count: self.tools.len(),
            });
        }

        // Structured output — schema enforcement of any kind requires
        // a non-None mode. ToolUseEmulation also implicitly needs
        // tool_use; that constraint is enforced by Capabilities::validate
        // at construction time, so we don't double-check here.
        if self.structured_output.is_some()
            && matches!(caps.supports_structured_output, StructuredOutputMode::None)
        {
            reasons.push(CompatibilityReason::StructuredOutputUnsupported);
        }

        // Thinking / reasoning — Auto + Budget both demand provider
        // support; Off is the no-op default. The OpenAI-compat
        // `enable_chat_template_thinking` field is a per-server
        // chat-template hint (Qwen3 / mlx_lm), separate from
        // ThinkingMode — providers ignore unknown fields, so we
        // don't gate on it.
        if !matches!(self.thinking, ThinkingMode::Off) && !caps.supports_thinking {
            reasons.push(CompatibilityReason::ThinkingUnsupported {
                mode: self.thinking,
            });
        }

        // Vision — any Image content block in any message requires
        // vision support. We scan all roles (user/assistant/system/tool)
        // because images can land anywhere in the history.
        let has_image = self.messages.iter().any(|m| {
            m.content()
                .iter()
                .any(|c| matches!(c, ContentBlock::Image { .. }))
        });
        if has_image && !caps.supports_vision {
            reasons.push(CompatibilityReason::VisionUnsupported);
        }

        // Context window — partial fix for B-32, ahead of full tokenizer
        // story (D-5 frozen). We use a chars/4 heuristic (typical
        // English BPE ratio) as the estimate. The estimate is
        // conservative-friendly: real provider tokenizers might pack
        // tighter for languages they're trained on but NOT looser, so
        // saying "estimated > max" is a real overflow signal even with
        // the rough estimate. False-negative tolerated (we let
        // borderline requests through to the provider, which still
        // catches them via wire-level 400 ContextTooLong); only flag
        // the obvious-overflow case to keep false-positive rate low.
        let prompt_chars = estimate_prompt_chars(self);
        let estimated_tokens = (prompt_chars / 4) as u32;
        if estimated_tokens > caps.max_context_tokens {
            reasons.push(CompatibilityReason::ContextWindowExceeded {
                estimated_prompt_tokens: estimated_tokens,
                max_context_tokens: caps.max_context_tokens,
            });
        }

        // Max output tokens — caller asks for more output than the
        // provider supports.
        if let Some(req_max) = self.max_output_tokens {
            if req_max > caps.max_output_tokens {
                reasons.push(CompatibilityReason::MaxOutputTokensExceeded {
                    requested: req_max,
                    max: caps.max_output_tokens,
                });
            }
        }

        if reasons.is_empty() {
            CompatibilityCheck::Compatible
        } else {
            CompatibilityCheck::Incompatible { reasons }
        }
    }
}

/// Estimate prompt size in chars by walking every text block of every
/// message. Used by the context-window pre-flight (no tokenizer
/// dependency). System prompt is included; the system prompt isn't in
/// `messages` but is sent on every call.
fn estimate_prompt_chars(req: &ChatRequest) -> usize {
    let mut total = req.system.as_ref().map(|s| s.len()).unwrap_or(0);
    for m in &req.messages {
        for c in m.content() {
            if let Some(t) = c.as_text() {
                total += t.len();
            }
        }
    }
    total
}

/// Result of [`ChatRequest::compatibility_check`]. Tells the routing
/// layer whether a particular provider can honour the request as-is.
///
/// We deliberately use a 2-state enum rather than 3-state
/// (Compatible / Skip / Reject):
/// per-candidate compatibility doesn't tell us whether *all*
/// candidates will fail. The routing layer collects reasons across
/// skipped candidates and surfaces "no compatible candidate"
/// after the loop ends — that's where the global "reject this
/// request shape" verdict belongs, not in this per-candidate helper.
///
/// `#[non_exhaustive]` so we can add e.g. `MaybeWithCaveat` later
/// (provider supports tools but with restricted schema, etc.) without
/// breaking the enum's SemVer contract — match arms must use `_ => …`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompatibilityCheck {
    /// This provider can serve the request as-is.
    Compatible,
    /// This provider can't serve the request — list the missing
    /// features. Routing layer typically logs and tries the next
    /// candidate; if all candidates return Incompatible, the layer
    /// surfaces the reasons as the final error.
    Incompatible { reasons: Vec<CompatibilityReason> },
}

/// Specific reason a provider's [`Capabilities`] don't cover a
/// [`ChatRequest`]'s feature requirements. Each variant is
/// independently match-able so callers can branch programmatically
/// (e.g., "drop tools and retry" vs "fail hard on context overflow")
/// without parsing strings.
///
/// `#[non_exhaustive]` because new capability axes will be added
/// (streaming required / specific quant / etc.) — match arms must
/// use `_ => …`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompatibilityReason {
    /// Request has tool specs but provider's `supports_tool_use=false`.
    ToolUseUnsupported { tool_count: usize },
    /// Request has `structured_output` schema but provider's
    /// `supports_structured_output=None`.
    StructuredOutputUnsupported,
    /// Request has `thinking != Off` but provider's
    /// `supports_thinking=false`.
    ThinkingUnsupported { mode: ThinkingMode },
    /// Request contains an Image content block but provider's
    /// `supports_vision=false`.
    VisionUnsupported,
    /// Estimated prompt size exceeds provider's `max_context_tokens`.
    /// Estimate uses a `chars/4` heuristic (no tokenizer dep until
    /// D-5 unfreezes). The estimate is conservative-toward-passing:
    /// flagged only when overflow is obvious.
    ContextWindowExceeded {
        /// Our estimate of the prompt's token count via chars/4.
        estimated_prompt_tokens: u32,
        /// Provider's declared cap.
        max_context_tokens: u32,
    },
    /// Caller's requested `max_output_tokens` exceeds provider's
    /// `max_output_tokens`. Set the request field smaller, or pick
    /// a model with bigger output cap.
    MaxOutputTokensExceeded { requested: u32, max: u32 },
}

impl std::fmt::Display for CompatibilityReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolUseUnsupported { tool_count } => write!(
                f,
                "request has {tool_count} tool(s) but provider does not support tool_use"
            ),
            Self::StructuredOutputUnsupported => write!(
                f,
                "request has structured_output schema but provider does not support structured output"
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
                "estimated prompt size {estimated_prompt_tokens} tokens (chars/4 heuristic) exceeds provider max_context_tokens={max_context_tokens}"
            ),
            Self::MaxOutputTokensExceeded { requested, max } => write!(
                f,
                "request max_output_tokens={requested} exceeds provider max={max}"
            ),
        }
    }
}

impl CompatibilityReason {
    /// Stable snake_case kind tag for telemetry / metric tags / Python
    /// callers that want to branch by string. Keeps enum variant names
    /// flexible (we may rename `ToolUseUnsupported` → `ToolsUnsupported`
    /// internally) while telemetry / dashboards stay stable.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ToolUseUnsupported { .. } => "tool_use",
            Self::StructuredOutputUnsupported => "structured_output",
            Self::ThinkingUnsupported { .. } => "thinking",
            Self::VisionUnsupported => "vision",
            Self::ContextWindowExceeded { .. } => "context_window",
            Self::MaxOutputTokensExceeded { .. } => "max_output_tokens",
        }
    }
}

/// Declarative requirements describing what features a caller needs
/// from a provider. Used by [`Capabilities::check_requirements`] for
/// **config-time** capability checks — e.g. "at startup, before any
/// request is built, verify that the provider configured for the
/// `critic` role supports tools + thinking".
///
/// Compared to [`ChatRequest::compatibility_check`]: this lets the
/// caller declare requirements without inventing a placeholder
/// `ChatRequest` (no fake messages, no prompt-content estimate). All
/// fields default to "I don't need this", so callers only set the
/// axes they actually care about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityRequirements {
    /// Caller will issue requests with `tools` set.
    pub requires_tools: bool,
    /// Caller will issue requests with image content blocks.
    pub requires_vision: bool,
    /// Caller will set `thinking != Off` on at least some requests.
    pub requires_thinking: bool,
    /// Caller will set `structured_output` on at least some requests.
    pub requires_structured_output: bool,
    /// Estimated upper bound on prompt size in tokens. `0` = no
    /// constraint to check. Useful for "I'll send up to 8k token
    /// prompts; does this provider's context window cover that?".
    pub estimated_max_prompt_tokens: u32,
    /// Largest `max_output_tokens` the caller will request. `0` = no
    /// constraint. Useful for "I'll ask for up to 4k output tokens
    /// sometimes; can the provider deliver?".
    pub estimated_max_output_tokens: u32,
}

impl Capabilities {
    /// Check whether these capabilities satisfy a set of caller-declared
    /// [`CapabilityRequirements`]. Returns the same
    /// [`CompatibilityCheck`] verdict as
    /// [`ChatRequest::compatibility_check`] so both API call-sites
    /// share the same downstream branching code.
    ///
    /// **Use this at config time** — before building any real request,
    /// to verify a configured provider can satisfy a role's needs.
    /// Avoids the "configure, send a real request, fail, fall back"
    /// loop in production.
    ///
    /// Aggregates ALL incompatibilities (no early-exit on first
    /// failure), same as `compatibility_check`.
    pub fn check_requirements(&self, req: &CapabilityRequirements) -> CompatibilityCheck {
        let mut reasons = Vec::new();

        if req.requires_tools && !self.supports_tool_use {
            // tool_count is unknown at config time — encode as 0 to
            // signal "user said they need tools, count not specified
            // yet". The Display message glosses over this.
            reasons.push(CompatibilityReason::ToolUseUnsupported { tool_count: 0 });
        }
        if req.requires_vision && !self.supports_vision {
            reasons.push(CompatibilityReason::VisionUnsupported);
        }
        if req.requires_thinking && !self.supports_thinking {
            // ThinkingMode::Auto is the conservative "caller wants any
            // thinking support" placeholder.
            reasons.push(CompatibilityReason::ThinkingUnsupported {
                mode: ThinkingMode::Auto,
            });
        }
        if req.requires_structured_output
            && matches!(self.supports_structured_output, StructuredOutputMode::None)
        {
            reasons.push(CompatibilityReason::StructuredOutputUnsupported);
        }
        if req.estimated_max_prompt_tokens > 0
            && req.estimated_max_prompt_tokens > self.max_context_tokens
        {
            reasons.push(CompatibilityReason::ContextWindowExceeded {
                estimated_prompt_tokens: req.estimated_max_prompt_tokens,
                max_context_tokens: self.max_context_tokens,
            });
        }
        if req.estimated_max_output_tokens > 0
            && req.estimated_max_output_tokens > self.max_output_tokens
        {
            reasons.push(CompatibilityReason::MaxOutputTokensExceeded {
                requested: req.estimated_max_output_tokens,
                max: self.max_output_tokens,
            });
        }

        if reasons.is_empty() {
            CompatibilityCheck::Compatible
        } else {
            CompatibilityCheck::Incompatible { reasons }
        }
    }
}

/// A message in the chat history. Mirrors the OpenAI/Anthropic role model.
///
/// **Note on `tool_calls`** — only present on `Assistant` messages. The
/// model emits tool calls; we send back the result as a `Tool` message
/// referencing the same `tool_call_id`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: Vec<ContentBlock>,
    },
    Assistant {
        // OpenAI omits `content` entirely on assistant messages that
        // are pure tool-call invocations. Without `default` here our
        // Deserialize fails on those messages. Audit
        // `tars-types-src-chat-13`.
        #[serde(default)]
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<crate::tools::ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
    },
    /// System messages are normally promoted out to `request.system`,
    /// but the variant exists for adapters that prefer to keep them
    /// inline (e.g. a long history replay).
    System {
        content: Vec<ContentBlock>,
    },
}

impl Message {
    /// Convenience — single-text user turn.
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentBlock::text(text)],
        }
    }

    /// Convenience — single-text assistant turn (no tool calls).
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![ContentBlock::text(text)],
            tool_calls: Vec::new(),
        }
    }

    /// Borrow the role's content list.
    pub fn content(&self) -> &[ContentBlock] {
        match self {
            Self::User { content }
            | Self::Assistant { content, .. }
            | Self::Tool { content, .. }
            | Self::System { content } => content,
        }
    }
}

/// Multi-modal content block. Wire format mirrors OpenAI/Anthropic:
/// `{"type": "text", "text": "..."}` / `{"type": "image", ...}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { mime: String, data: ImageData },
}

impl ContentBlock {
    /// Convenience constructor — most callers just want a text block.
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    /// Borrow text content if this is a text block.
    pub fn as_text(&self) -> Option<&str> {
        if let Self::Text { text } = self {
            Some(text)
        } else {
            None
        }
    }
}

/// How an image arrives. URL = remote / model-fetched. Inline = base64
/// bytes. We never send raw `Vec<u8>` over the wire to the LLM; provider
/// adapters base64-encode at the boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageData {
    Url(String),
    Base64(String),
}

impl ImageData {
    /// SHA-256 hex digest of the *descriptor* — the URL string for
    /// `Url` or the encoded form for `Base64`. **Does not** fetch the
    /// remote bytes for `Url`, so two different images served from the
    /// same URL hash identically. Audit `tars-types-src-chat-15`:
    /// previously named `content_hash`, which mis-implied the function
    /// hashed the actual image bytes and risked stale cache hits.
    pub fn descriptor_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        match self {
            Self::Url(u) => h.update(u.as_bytes()),
            Self::Base64(b) => h.update(b.as_bytes()),
        }
        let bytes = h.finalize();
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TenantId;

    fn fake_tenant() -> TenantId {
        TenantId::new("test")
    }

    #[test]
    fn user_builder_creates_minimal_request() {
        let r = ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), "hi");
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
    fn descriptor_hash_is_stable_and_only_descriptor() {
        let _ = fake_tenant();
        let a = ImageData::Url("https://x/y".into()).descriptor_hash();
        let b = ImageData::Url("https://x/y".into()).descriptor_hash();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    // ── compatibility_check tests (B-31) ────────────────────────────

    use crate::capabilities::{Capabilities, Modality, PromptCacheKind};
    use crate::schema::JsonSchema;
    use crate::usage::Pricing;
    use std::collections::HashSet;

    fn caps_minimal() -> Capabilities {
        let mut text_only = HashSet::new();
        text_only.insert(Modality::Text);
        Capabilities {
            max_context_tokens: 32_768,
            max_output_tokens: 4096,
            supports_tool_use: false,
            supports_parallel_tool_calls: false,
            supports_structured_output: StructuredOutputMode::None,
            supports_vision: false,
            supports_thinking: false,
            supports_cancel: true,
            prompt_cache: PromptCacheKind::None,
            streaming: true,
            modalities_in: text_only.clone(),
            modalities_out: text_only,
            pricing: Pricing::default(),
        }
    }

    fn caps_full() -> Capabilities {
        let mut both = HashSet::new();
        both.insert(Modality::Text);
        both.insert(Modality::Image);
        Capabilities {
            max_context_tokens: 200_000,
            max_output_tokens: 8192,
            supports_tool_use: true,
            supports_parallel_tool_calls: true,
            supports_structured_output: StructuredOutputMode::StrictSchema,
            supports_vision: true,
            supports_thinking: true,
            supports_cancel: true,
            prompt_cache: PromptCacheKind::ImplicitPrefix { min_tokens: 1024 },
            streaming: true,
            modalities_in: both.clone(),
            modalities_out: both,
            pricing: Pricing::default(),
        }
    }

    #[test]
    fn compat_text_only_request_passes_minimal_provider() {
        let req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        assert!(matches!(
            req.compatibility_check(&caps_minimal()),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn compat_tools_blocked_by_no_tool_support() {
        use crate::tools::ToolSpec;
        let mut req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        req.tools.push(ToolSpec {
            name: "x".into(),
            description: "x".into(),
            input_schema: JsonSchema::loose(serde_json::json!({"type":"object"})),
        });
        match req.compatibility_check(&caps_minimal()) {
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

    #[test]
    fn compat_tools_pass_when_provider_supports() {
        use crate::tools::ToolSpec;
        let mut req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        req.tools.push(ToolSpec {
            name: "x".into(),
            description: "x".into(),
            input_schema: JsonSchema::loose(serde_json::json!({"type":"object"})),
        });
        assert!(matches!(
            req.compatibility_check(&caps_full()),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn compat_thinking_auto_blocked_when_not_supported() {
        let mut req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        req.thinking = ThinkingMode::Auto;
        match req.compatibility_check(&caps_minimal()) {
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
        let req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        assert!(matches!(req.thinking, ThinkingMode::Off));
        assert!(matches!(
            req.compatibility_check(&caps_minimal()),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn compat_structured_output_blocked_by_none_mode() {
        let mut req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        req.structured_output = Some(JsonSchema::loose(serde_json::json!({"type":"object"})));
        match req.compatibility_check(&caps_minimal()) {
            CompatibilityCheck::Incompatible { reasons } => {
                assert!(
                    reasons
                        .iter()
                        .any(|r| matches!(r, CompatibilityReason::StructuredOutputUnsupported))
                );
            }
            _ => panic!("expected Incompatible"),
        }
    }

    #[test]
    fn compat_image_content_blocked_by_no_vision() {
        let req = ChatRequest {
            model: ModelHint::Explicit("m".into()),
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
            thinking: ThinkingMode::default(),
            enable_chat_template_thinking: None,
        };
        match req.compatibility_check(&caps_minimal()) {
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
        use crate::tools::ToolSpec;
        let mut req = ChatRequest {
            model: ModelHint::Explicit("m".into()),
            system: None,
            messages: vec![Message::User {
                content: vec![ContentBlock::Image {
                    mime: "image/png".into(),
                    data: ImageData::Url("https://x".into()),
                }],
            }],
            tools: Vec::new(),
            tool_choice: ToolChoice::default(),
            structured_output: Some(JsonSchema::loose(serde_json::json!({}))),
            max_output_tokens: None,
            temperature: None,
            stop_sequences: Vec::new(),
            seed: None,
            cache_directives: Vec::new(),
            thinking: ThinkingMode::Auto,
            enable_chat_template_thinking: None,
        };
        req.tools.push(ToolSpec {
            name: "x".into(),
            description: "x".into(),
            input_schema: JsonSchema::loose(serde_json::json!({})),
        });
        match req.compatibility_check(&caps_minimal()) {
            CompatibilityCheck::Incompatible { reasons } => {
                assert_eq!(
                    reasons.len(),
                    4,
                    "expected all 4 reasons collected, got: {reasons:?}"
                );
                // verify each kind shows up exactly once
                let kinds: std::collections::HashSet<_> =
                    reasons.iter().map(|r| r.kind()).collect();
                assert_eq!(
                    kinds,
                    ["tool_use", "structured_output", "thinking", "vision"]
                        .into_iter()
                        .collect()
                );
            }
            _ => panic!("expected Incompatible"),
        }
    }

    // ── Context window + max_output checks (new in B-31 v2) ──────

    #[test]
    fn compat_context_window_exceeded_flagged() {
        // Build a request whose prompt clearly exceeds 32k tokens
        // (chars/4 estimate). 32k tokens × 4 chars/tok ≈ 128k chars;
        // pad to 200k chars to be obviously over.
        let mut req = ChatRequest::user(ModelHint::Explicit("m".into()), "x".repeat(200_000));
        req.system = None;
        match req.compatibility_check(&caps_minimal()) {
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
        let mut req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        req.max_output_tokens = Some(8192); // caps_minimal has max=4096
        match req.compatibility_check(&caps_minimal()) {
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
        let mut req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        req.max_output_tokens = Some(4096); // exactly at the cap
        assert!(matches!(
            req.compatibility_check(&caps_minimal()),
            CompatibilityCheck::Compatible
        ));
    }

    // ── Boundary cases ─────────────────────────────────────────────

    /// All-zeroed capabilities for boundary testing — represents
    /// "the most-restricted possible provider". Capabilities itself
    /// doesn't impl `Default` (would let invalid configs through), so
    /// build via `text_only_baseline` then zero out supports_* fields.
    fn caps_zero() -> Capabilities {
        let mut c = Capabilities::text_only_baseline(Pricing::default());
        c.max_context_tokens = 0;
        c.max_output_tokens = 0;
        c.supports_tool_use = false;
        c.supports_parallel_tool_calls = false;
        c.supports_structured_output = StructuredOutputMode::None;
        c.supports_vision = false;
        c.supports_thinking = false;
        c.streaming = false;
        c
    }

    #[test]
    fn compat_baseline_capabilities_text_only_request_passes() {
        // `text_only_baseline()` provides 32k context + 4k output +
        // text-only modalities. A trivial text request should pass.
        let caps = Capabilities::text_only_baseline(Pricing::default());
        let req = ChatRequest::user(ModelHint::Explicit("m".into()), "hi");
        match req.compatibility_check(&caps) {
            CompatibilityCheck::Compatible => {}
            CompatibilityCheck::Incompatible { reasons } => {
                panic!("expected Compatible, got: {reasons:?}");
            }
        }
    }

    #[test]
    fn compat_zero_capability_provider_with_full_feature_request_aggregates_all() {
        // Hard adversarial case: provider with everything OFF, request
        // with everything ON. Want: ALL reasons aggregated, no early-
        // exit on first failure. Use 6 features simultaneously.
        use crate::tools::ToolSpec;
        let zero_caps = caps_zero(); // everything off, max_*_tokens = 0
        let req = ChatRequest {
            model: ModelHint::Explicit("m".into()),
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
                input_schema: JsonSchema::loose(serde_json::json!({})),
            }],
            tool_choice: ToolChoice::default(),
            structured_output: Some(JsonSchema::loose(serde_json::json!({}))),
            max_output_tokens: Some(99_999), // exceeds default max=0
            temperature: None,
            stop_sequences: Vec::new(),
            seed: None,
            cache_directives: Vec::new(),
            thinking: ThinkingMode::Auto,
            enable_chat_template_thinking: None,
        };
        match req.compatibility_check(&zero_caps) {
            CompatibilityCheck::Incompatible { reasons } => {
                let kinds: std::collections::HashSet<_> =
                    reasons.iter().map(|r| r.kind()).collect();
                // Expect all 6 reason kinds present
                assert!(kinds.contains("tool_use"));
                assert!(kinds.contains("structured_output"));
                assert!(kinds.contains("thinking"));
                assert!(kinds.contains("vision"));
                assert!(kinds.contains("context_window"));
                assert!(kinds.contains("max_output_tokens"));
                assert_eq!(kinds.len(), 6, "expected all 6 axes flagged: {kinds:?}");
            }
            _ => panic!("expected Incompatible with all 6 reasons"),
        }
    }

    // ── check_requirements (config-time API) ─────────────────────

    #[test]
    fn requirements_default_is_empty_and_passes_minimal_caps() {
        let reqs = CapabilityRequirements::default();
        assert!(matches!(
            caps_minimal().check_requirements(&reqs),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn requirements_tools_blocked_by_no_tool_caps() {
        let reqs = CapabilityRequirements {
            requires_tools: true,
            ..Default::default()
        };
        match caps_minimal().check_requirements(&reqs) {
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
        let reqs = CapabilityRequirements {
            requires_tools: true,
            requires_vision: true,
            requires_thinking: true,
            requires_structured_output: true,
            estimated_max_prompt_tokens: 100_000, // > caps_minimal's 32k
            estimated_max_output_tokens: 8_000,   // > caps_minimal's 4096
        };
        match caps_minimal().check_requirements(&reqs) {
            CompatibilityCheck::Incompatible { reasons } => {
                let kinds: std::collections::HashSet<_> =
                    reasons.iter().map(|r| r.kind()).collect();
                assert_eq!(kinds.len(), 6, "expected all 6 axes flagged: {kinds:?}");
            }
            _ => panic!("expected Incompatible"),
        }
    }

    #[test]
    fn requirements_zero_estimated_tokens_means_no_check() {
        // estimated_max_prompt_tokens=0 = "I'll worry about size later".
        // Should NOT flag context overflow even when max_context_tokens=0.
        let reqs = CapabilityRequirements {
            estimated_max_prompt_tokens: 0,
            estimated_max_output_tokens: 0,
            ..Default::default()
        };
        let mut caps = caps_minimal();
        caps.max_context_tokens = 0;
        caps.max_output_tokens = 0;
        // Default everything else — still Compatible.
        assert!(matches!(
            caps.check_requirements(&reqs),
            CompatibilityCheck::Compatible
        ));
    }

    #[test]
    fn requirements_full_caps_passes_demanding_requirements() {
        let reqs = CapabilityRequirements {
            requires_tools: true,
            requires_vision: true,
            requires_thinking: true,
            requires_structured_output: true,
            estimated_max_prompt_tokens: 100_000, // < caps_full's 200k
            estimated_max_output_tokens: 4_000,   // < caps_full's 8k
        };
        assert!(matches!(
            caps_full().check_requirements(&reqs),
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
        assert!(s.contains("chars/4"));
    }
}
