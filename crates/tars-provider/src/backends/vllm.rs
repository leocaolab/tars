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
use crate::http_base::{HttpProviderBase, HttpProviderExtras};
use crate::provider::LlmProvider;

/// Default vLLM base URL — matches Python `DEFAULT_BASE_URL`.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8000/v1";

/// Coerce `Some("")` (or `None`) to the default base URL.
///
/// Mirrors the auth normalization below: an empty configured URL is
/// almost certainly a config oversight, not an instruction to connect
/// to the empty string. Falling back to the default produces a
/// localhost connection failure (debuggable) instead of a malformed
/// request URL.
fn normalize_base_url(base_url: Option<String>) -> String {
    base_url
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// Coerce empty / whitespace-only inline credentials to `Auth::None`.
///
/// vLLM by default doesn't authenticate, and our OpenAI adapter would
/// otherwise try to send `Bearer ` (empty/whitespace), which some
/// gateways reject with unclear errors. Mirrors Python
/// `api_key="EMPTY"` substitution.
fn normalize_auth(auth: Auth) -> Auth {
    match auth {
        Auth::Secret { secret: tars_types::SecretRef::Inline { ref value } }
            if value.expose().trim().is_empty() =>
        {
            Auth::None
        }
        other => other,
    }
}

/// Build a provider configured for a vLLM server.
///
/// `auth` is normalized: an empty / whitespace-only inline string
/// becomes `Auth::None` (vLLM by default doesn't authenticate). The
/// Python client sends `api_key="EMPTY"` — same idea, expressed as
/// the absence of an auth header.
pub fn vllm(
    id: impl Into<tars_types::ProviderId>,
    base_url: Option<String>,
    auth: Auth,
    extras: HttpProviderExtras,
    capability_overrides: tars_config::CapabilitiesOverrides,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
) -> Arc<dyn LlmProvider> {
    let url = normalize_base_url(base_url);
    let normalized_auth = normalize_auth(auth);
    let mut caps = local_openai_compat_capabilities();
    capability_overrides.apply_to(&mut caps);
    OpenAiProviderBuilder::new(id, normalized_auth)
        .base_url(url)
        .extras(extras)
        // Override capabilities: local servers don't have implicit
        // prefix cache; thinking is model-specific (Qwen Thinker has
        // it, but that's per-deployment, not per-vLLM).
        .capabilities(caps)
        .build(http, auth_resolver)
}

/// Convenience for the common case: localhost:8000, no auth.
pub fn vllm_local(
    id: impl Into<tars_types::ProviderId>,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
) -> Arc<dyn LlmProvider> {
    vllm(
        id,
        None,
        Auth::None,
        HttpProviderExtras::default(),
        tars_config::CapabilitiesOverrides::default(),
        http,
        auth_resolver,
    )
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
        // Assumes the vLLM server has `guided_json` enabled (it is the
        // default in modern vLLM, but older or stripped builds may not
        // ship it). If the deployment doesn't support guided_json,
        // structured-output requests will fail at runtime with a 4xx
        // from the server — override this capability via the builder.
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
    use tars_types::SecretRef;

    // --- normalize_base_url -------------------------------------------------

    #[test]
    fn base_url_none_falls_back_to_default() {
        assert_eq!(normalize_base_url(None), DEFAULT_BASE_URL);
    }

    #[test]
    fn base_url_empty_string_falls_back_to_default() {
        assert_eq!(normalize_base_url(Some(String::new())), DEFAULT_BASE_URL);
    }

    #[test]
    fn base_url_custom_value_passes_through() {
        let custom = "http://10.0.0.5:9000/v1".to_string();
        assert_eq!(normalize_base_url(Some(custom.clone())), custom);
    }

    #[test]
    fn base_url_malformed_passes_through_unchanged() {
        // We don't parse here — that's the HTTP layer's job. We only
        // guard against the "silently empty" failure mode.
        let weird = "not a url".to_string();
        assert_eq!(normalize_base_url(Some(weird.clone())), weird);
    }

    // --- normalize_auth -----------------------------------------------------

    #[test]
    fn auth_empty_inline_becomes_none() {
        let out = normalize_auth(Auth::inline(String::new()));
        assert!(matches!(out, Auth::None));
    }

    #[test]
    fn auth_whitespace_inline_becomes_none() {
        let out = normalize_auth(Auth::inline("  \t\n"));
        assert!(matches!(out, Auth::None));
    }

    #[test]
    fn auth_nonempty_inline_passes_through() {
        let out = normalize_auth(Auth::inline("sk-real-key"));
        match out {
            Auth::Secret { secret: SecretRef::Inline { value } } => {
                assert_eq!(value.expose(), "sk-real-key");
            }
            other => panic!("expected inline secret, got {other:?}"),
        }
    }

    #[test]
    fn auth_env_variant_passes_through() {
        let out = normalize_auth(Auth::env("VLLM_KEY"));
        match out {
            Auth::Secret { secret: SecretRef::Env { var } } => {
                assert_eq!(var, "VLLM_KEY");
            }
            other => panic!("expected env secret, got {other:?}"),
        }
    }

    #[test]
    fn auth_none_passes_through() {
        assert!(matches!(normalize_auth(Auth::None), Auth::None));
    }

    #[test]
    fn auth_delegate_passes_through() {
        assert!(matches!(normalize_auth(Auth::Delegate), Auth::Delegate));
    }

    // --- builder integration ------------------------------------------------

    #[tokio::test]
    async fn vllm_local_capability_profile() {
        let http = HttpProviderBase::default_arc()
            .expect("Failed to create default HTTP provider base");
        let p = vllm_local("vllm_test", http, basic());
        // Capability profile should match `local_openai_compat_capabilities`.
        // (URL/auth correctness is covered by the normalize_* unit tests.)
        let caps = p.capabilities();
        assert!(matches!(
            caps.prompt_cache,
            tars_types::PromptCacheKind::None
        ));
        assert!(!caps.supports_thinking);
        assert!(caps.supports_tool_use);
    }

    #[tokio::test]
    async fn vllm_accepts_empty_inline_auth() {
        let http = HttpProviderBase::default_arc()
            .expect("Failed to create default HTTP provider base");
        // Construction must not panic when given an empty inline auth
        // — normalize_auth coerces it to Auth::None upstream of the
        // OpenAiProvider's resolver. (The coercion itself is verified
        // by `auth_empty_inline_becomes_none` above.)
        let _ = vllm(
            "vllm_t",
            None,
            Auth::inline(String::new()),
            HttpProviderExtras::default(),
            tars_config::CapabilitiesOverrides::default(),
            http,
            basic(),
        );
    }
}
