//! Canonical request shape — what every Provider accepts.

use serde::{Deserialize, Serialize};

use crate::cache::CacheDirective;
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
}

impl ChatRequest {
    /// Minimal builder for the common single-user-turn case.
    pub fn user(model: ModelHint, prompt: impl Into<String>) -> Self {
        Self {
            model,
            system: None,
            messages: vec![Message::User { content: vec![ContentBlock::text(prompt)] }],
            tools: Vec::new(),
            tool_choice: ToolChoice::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: Vec::new(),
            seed: None,
            cache_directives: Vec::new(),
            thinking: ThinkingMode::default(),
        }
    }

    /// Set system prompt (chainable).
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
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
        Self::User { content: vec![ContentBlock::text(text)] }
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
            Self::User { content } | Self::Assistant { content, .. } | Self::Tool { content, .. } | Self::System { content } => content,
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
        if let Self::Text { text } = self { Some(text) } else { None }
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
        let r = ChatRequest::user(
            ModelHint::Explicit("gpt-4o".into()),
            "hi",
        );
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
}
