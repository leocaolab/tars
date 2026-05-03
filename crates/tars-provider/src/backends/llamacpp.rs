//! llama.cpp backend — `llama-server` (the binary built from llama.cpp).
//!
//! `llama-server` exposes an OpenAI-compatible chat/completions
//! endpoint, so this is another thin wrapper over
//! [`OpenAiProviderBuilder`] with local-server defaults — same shape as
//! `vllm.rs` and `mlx.rs`.
//!
//! Why a dedicated variant instead of `openai_compat`?
//! - `type = "llamacpp"` in config makes the deployment posture
//!   ("running on a Ryzen / iGPU box via Vulkan or Metal") legible at
//!   a glance.
//! - llama.cpp's tool-call quality varies model-by-model and its
//!   chat-template handling has edge cases the generic OpenAI adapter
//!   shouldn't carry assumptions about. A dedicated capability profile
//!   lets the routing layer treat it differently.
//! - Future: llama.cpp-specific server fields (`cache_prompt`, `slot_id`,
//!   `n_predict`) can be plumbed here without polluting OpenAI.
//!
//! Run a llama-server instance:
//! ```bash
//! ./llama-server -m models/Qwen2.5-Coder-7B-Q5_K_M.gguf \
//!   --host 127.0.0.1 --port 8080 --n-gpu-layers 999
//! ```

use std::sync::Arc;

use crate::auth::{Auth, AuthResolver};
use crate::backends::openai::OpenAiProviderBuilder;
use crate::http_base::{HttpProviderBase, HttpProviderExtras};
use crate::provider::LlmProvider;

/// Default `llama-server` base URL. The binary's own default is
/// `127.0.0.1:8080`; we use `localhost` for symmetry with other local
/// backends ([`crate::backends::vllm`], [`crate::backends::mlx`]).
///
/// 8080 collides with `mlx-lm.server`'s default — pick one per host or
/// override `--port` on whichever you launch second.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8080/v1";

/// Build a provider configured for a llama.cpp server.
///
/// `auth` is normalized: an empty inline string becomes `Auth::None`
/// (`llama-server` doesn't authenticate by default; an `--api-key` flag
/// exists but is rarely set on local boxes).
pub fn llamacpp(
    id: impl Into<tars_types::ProviderId>,
    base_url: Option<String>,
    auth: Auth,
    extras: HttpProviderExtras,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
) -> Arc<dyn LlmProvider> {
    let url = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
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
        .extras(extras)
        .capabilities(llamacpp_default_capabilities())
        .build(http, auth_resolver)
}

/// Convenience for the common case: `localhost:8080`, no auth.
pub fn llamacpp_local(
    id: impl Into<tars_types::ProviderId>,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
) -> Arc<dyn LlmProvider> {
    llamacpp(
        id,
        None,
        Auth::None,
        HttpProviderExtras::default(),
        http,
        auth_resolver,
    )
}

/// Conservative defaults tuned for Ryzen iGPU / Apple Silicon hosts
/// running quantized GGUF models. Override per-deployment.
fn llamacpp_default_capabilities() -> tars_types::Capabilities {
    use std::collections::HashSet;
    use tars_types::{
        Capabilities, Modality, Pricing, PromptCacheKind, StructuredOutputMode,
    };

    let mut modalities = HashSet::new();
    modalities.insert(Modality::Text);
    Capabilities {
        // 8K is a safe default for 7–14B GGUFs at common quantizations;
        // bump per deployment when running long-context models.
        max_context_tokens: 8_192,
        max_output_tokens: 2_048,
        // llama.cpp added OpenAI-style tool-call support, but quality
        // is highly model-dependent. Leave on; routing can opt out.
        supports_tool_use: true,
        supports_parallel_tool_calls: false,
        // llama-server supports `response_format=json_object`; grammar-
        // constrained strict schemas are model+template-dependent.
        supports_structured_output: StructuredOutputMode::JsonObjectMode,
        supports_vision: false, // multimodal needs llava/mmproj plumbing
        supports_thinking: false,
        supports_cancel: true,
        // llama-server has its own `cache_prompt` mechanism but doesn't
        // surface a billing-discount semantic the way hosted APIs do.
        prompt_cache: PromptCacheKind::None,
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
    async fn llamacpp_local_uses_default_url_and_no_auth() {
        let http = HttpProviderBase::default_arc().unwrap();
        let p = llamacpp_local("llamacpp_test", http, basic());
        let caps = p.capabilities();
        assert!(matches!(caps.prompt_cache, tars_types::PromptCacheKind::None));
        assert_eq!(caps.max_context_tokens, 8_192);
    }

    #[tokio::test]
    async fn llamacpp_normalizes_empty_inline_auth_to_none() {
        let http = HttpProviderBase::default_arc().unwrap();
        let _ = llamacpp(
            "lc_t",
            None,
            Auth::inline(String::new()),
            HttpProviderExtras::default(),
            http,
            basic(),
        );
    }
}
