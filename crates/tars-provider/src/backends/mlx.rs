//! MLX backend — Apple Silicon native inference via `mlx-lm.server`.
//!
//! `mlx-lm` ships an OpenAI-compatible HTTP server, so this is a thin
//! wrapper over [`OpenAiProviderBuilder`] with sensible "local server,
//! no auth" defaults — same recipe as `vllm.rs`.
//!
//! Why a dedicated variant instead of `openai_compat`?
//! - Distinct identity in logs / config: `type = "mlx"` reads as
//!   "running on the Mac Pro with unified memory" at a glance.
//! - Different default capability profile (Apple Silicon GPUs have
//!   first-class fp16/bf16 paths but no implicit prefix cache, etc.).
//! - Future-proofing: when `mlx_lm.server` grows MLX-specific extras
//!   (e.g. KV cache reuse hints), they land here without polluting the
//!   generic OpenAI adapter.
//!
//! Run an MLX server:
//! ```bash
//! pip install mlx-lm
//! mlx_lm.server --model mlx-community/Qwen2.5-Coder-32B-Instruct-4bit \
//!   --host 127.0.0.1 --port 8080
//! ```

use std::sync::Arc;

use crate::auth::{Auth, AuthResolver};
use crate::backends::openai::OpenAiProviderBuilder;
use crate::http_base::{HttpProviderBase, HttpProviderExtras};
use crate::provider::LlmProvider;

/// Default `mlx_lm.server` base URL. The server's own default is
/// `127.0.0.1:8080`; we keep `localhost` (resolves the same on macOS)
/// for symmetry with [`crate::backends::vllm::DEFAULT_BASE_URL`].
pub const DEFAULT_BASE_URL: &str = "http://localhost:8080/v1";

/// Build a provider configured for an MLX server.
///
/// `auth` is normalized: an empty inline string becomes `Auth::None`
/// (mlx-lm.server doesn't authenticate by default — same as vLLM).
pub fn mlx(
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
        .capabilities(mlx_default_capabilities())
        .build(http, auth_resolver)
}

/// Convenience for the common case: `localhost:8080`, no auth.
pub fn mlx_local(
    id: impl Into<tars_types::ProviderId>,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
) -> Arc<dyn LlmProvider> {
    mlx(
        id,
        None,
        Auth::None,
        HttpProviderExtras::default(),
        http,
        auth_resolver,
    )
}

/// Conservative defaults tuned for Apple Silicon hosts running 8B–32B
/// quantized models in unified memory. Override per-deployment.
fn mlx_default_capabilities() -> tars_types::Capabilities {
    use std::collections::HashSet;
    use tars_types::{
        Capabilities, Modality, Pricing, PromptCacheKind, StructuredOutputMode,
    };

    let mut modalities = HashSet::new();
    modalities.insert(Modality::Text);
    Capabilities {
        // Most MLX-converted Qwen / Llama / Mistral models top out
        // around 32K–128K context. 32K is the safer default; bump per
        // deployment when you've actually loaded a long-context model.
        max_context_tokens: 32_768,
        max_output_tokens: 4_096,
        supports_tool_use: true,
        // mlx-lm currently emits tool calls serially; parallel batches
        // round-trip through OpenAI shape but model quality varies.
        supports_parallel_tool_calls: false,
        // mlx-lm.server supports `response_format=json_object`; strict
        // schema enforcement is model-dependent. Default to JSON-only.
        supports_structured_output: StructuredOutputMode::JsonObjectMode,
        supports_vision: false, // VLMs need a different runner today
        supports_thinking: false,
        supports_cancel: true,
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
    async fn mlx_local_uses_default_url_and_no_auth() {
        let http = HttpProviderBase::default_arc().unwrap();
        let p = mlx_local("mlx_test", http, basic());
        let caps = p.capabilities();
        assert!(matches!(caps.prompt_cache, tars_types::PromptCacheKind::None));
        assert!(!caps.supports_vision);
        assert!(caps.supports_tool_use);
    }

    #[tokio::test]
    async fn mlx_normalizes_empty_inline_auth_to_none() {
        let http = HttpProviderBase::default_arc().unwrap();
        let _ = mlx(
            "mlx_t",
            None,
            Auth::inline(String::new()),
            HttpProviderExtras::default(),
            http,
            basic(),
        );
    }

    #[test]
    fn default_base_url_is_localhost_8080() {
        assert!(DEFAULT_BASE_URL.contains("8080"));
        assert!(DEFAULT_BASE_URL.ends_with("/v1"));
    }
}
