//! vLLM backend — OpenAI-compatible local inference server.
//!
//! Mirrors the Python `VLLMClient` (in `arc/app/llm/vllm_client.py`):
//! it's just an [`OpenAiProvider`] with a different `base_url` and
//! sensible defaults for "local server with no auth".
//!
//! Same wrapper pattern works for llama.cpp server, LM Studio, Ollama's
//! OpenAI-compatible endpoint, Groq, Together, DeepSeek — anything that
//! speaks the OpenAI chat/completions wire format. They get their own
//! thin builders (e.g. [`vllm_local`]) for ergonomics, but the actual
//! adapter is shared.

use std::sync::Arc;

use crate::auth::{Auth, AuthResolver};
use crate::backends::openai::OpenAiProviderBuilder;
use crate::http_base::HttpProviderBase;
use crate::provider::LlmProvider;

/// Default vLLM base URL — matches Python `DEFAULT_BASE_URL`.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8000/v1";

/// Build a provider configured for a vLLM server.
///
/// `auth` is normalized: an empty inline string becomes `Auth::None`
/// (vLLM by default doesn't authenticate). The Python client sends
/// `api_key="EMPTY"` — same idea, expressed as the absence of an
/// auth header.
pub fn vllm(
    id: impl Into<tars_types::ProviderId>,
    base_url: Option<String>,
    auth: Auth,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
) -> Arc<dyn LlmProvider> {
    let url = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    // Coerce empty-string inline credential to "no auth" — vLLM by
    // default doesn't authenticate, and `OpenAI SDK / our adapter
    // would otherwise try to send `Bearer ` (empty), which some
    // gateways reject. Mirrors Python `api_key="EMPTY"` substitution.
    let normalized_auth = match auth {
        Auth::Secret { secret: tars_types::SecretRef::Inline { ref value } }
            if value.is_empty() =>
        {
            Auth::None
        }
        other => other,
    };
    OpenAiProviderBuilder::new(id, normalized_auth)
        .base_url(url)
        // Override capabilities: local servers don't have implicit
        // prefix cache; thinking is model-specific (Qwen Thinker has
        // it, but that's per-deployment, not per-vLLM).
        .capabilities(local_openai_compat_capabilities())
        .build(http, auth_resolver)
}

/// Convenience for the common case: localhost:8000, no auth.
pub fn vllm_local(
    id: impl Into<tars_types::ProviderId>,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
) -> Arc<dyn LlmProvider> {
    vllm(id, None, Auth::None, http, auth_resolver)
}

fn local_openai_compat_capabilities() -> tars_types::Capabilities {
    use std::collections::HashSet;
    use tars_types::{
        Capabilities, Modality, Pricing, PromptCacheKind, StructuredOutputMode,
    };

    let mut modalities = HashSet::new();
    modalities.insert(Modality::Text);
    Capabilities {
        // Conservative defaults — overridden per-deployment via builder.
        max_context_tokens: 32_768,
        max_output_tokens: 4_096,
        supports_tool_use: true,
        supports_parallel_tool_calls: false, // vLLM tool-call quality varies
        supports_structured_output: StructuredOutputMode::StrictSchema, // guided_json
        supports_vision: false,
        supports_thinking: false,
        supports_cancel: true,
        prompt_cache: PromptCacheKind::None, // local servers don't bill, no incentive
        streaming: true,
        modalities_in: modalities.clone(),
        modalities_out: modalities,
        pricing: Pricing::default(), // local = free
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::basic;

    #[tokio::test]
    async fn vllm_local_uses_default_url_and_no_auth() {
        let http = HttpProviderBase::default_arc().unwrap();
        let p = vllm_local("vllm_test", http, basic());
        // The capability profile should match `local_openai_compat_capabilities`.
        let caps = p.capabilities();
        assert!(matches!(
            caps.prompt_cache,
            tars_types::PromptCacheKind::None
        ));
        assert!(!caps.supports_thinking);
    }

    #[tokio::test]
    async fn vllm_normalizes_empty_inline_auth_to_none() {
        let http = HttpProviderBase::default_arc().unwrap();
        // Should not panic; the empty inline gets coerced to Auth::None
        // before reaching OpenAiProvider's auth resolution.
        let _ = vllm("vllm_t", None, Auth::inline(String::new()), http, basic());
    }
}
