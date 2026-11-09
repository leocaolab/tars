//! Builds an OpenAI provider for llama.cpp's `llama-server`, with no key by default. It is its
//! own kind rather than `openai_compat` so it has its own capability profile: its tool calls and
//! chat templates vary by model.
//!
//! ```bash
//! ./llama-server -m models/Qwen2.5-Coder-7B-Q5_K_M.gguf \
//!   --host 127.0.0.1 --port 8080 --n-gpu-layers 999
//! ```

use crate::provider::auth::Auth;
use crate::provider::backends::openai::OpenAiProviderBuilder;
use crate::provider::http_base::HttpOverrides;

/// The `llama-server` default address. `mlx_lm.server` defaults to the same port, so two on one
/// host need another `--port`.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8080/v1";

/// Returns the builder of a provider for this server, with the user's declared capabilities
/// written over the data file's; the caller sets more on it (the wire recorder) before building.
pub(crate) fn llamacpp_builder(
    id: tars_types::ProviderId,
    base_url: Option<String>,
    auth: Auth,
    extras: HttpOverrides,
    capability_overrides: tars_config::CapabilitiesOverrides,
    model_catalog: &tars_config::ModelCatalog<'_>,
) -> OpenAiProviderBuilder {
    let url = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let normalized_auth = match auth {
        Auth::Secret {
            secret: tars_types::SecretRef::Inline { ref value },
        } if value.is_empty() => Auth::None,
        other => other,
    };
    let caps = capability_overrides.apply_to(model_catalog.capabilities_for("llamacpp", ""));
    let output_limits = tars_config::OutputLimitRule::catalog("llamacpp")
        .configured(capability_overrides.max_output_tokens);
    OpenAiProviderBuilder::new(id, normalized_auth)
        .base_url(url)
        .extras(extras)
        .capabilities(caps)
        .output_limits(output_limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::LlmProvider;
    use crate::provider::auth::basic;
    use crate::provider::http_base::HttpClient;
    use tars_types::ProviderId;

    fn built(auth: Auth) -> std::sync::Arc<dyn LlmProvider> {
        llamacpp_builder(
            ProviderId::new("t"),
            None,
            auth,
            HttpOverrides::default(),
            tars_config::CapabilitiesOverrides::default(),
            tars_config::catalog(),
        )
        .build(HttpClient::default_arc().unwrap(), basic())
    }

    #[tokio::test]
    async fn a_llamacpp_provider_has_the_data_files_profile() {
        let p = built(Auth::None);
        let caps = p.capabilities();
        assert!(matches!(
            caps.prompt_cache,
            tars_types::PromptCacheKind::None
        ));
        assert_eq!(caps.max_context_tokens, None);
    }

    #[tokio::test]
    async fn llamacpp_normalizes_empty_inline_auth_to_none() {
        let _ = built(Auth::inline(String::new()));
    }
}
