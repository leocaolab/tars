//! Builds an OpenAI provider for `mlx_lm.server` on Apple Silicon, with no key by default. It is
//! its own kind rather than `openai_compat` so it has its own name in logs and its own capability
//! profile, which has no prompt cache.
//!
//! ```bash
//! pip install mlx-lm
//! mlx_lm.server --model mlx-community/Qwen2.5-Coder-32B-Instruct-4bit \
//!   --host 127.0.0.1 --port 8080
//! ```

use crate::provider::auth::Auth;
use crate::provider::backends::openai::OpenAiProviderBuilder;
use crate::provider::http_base::HttpOverrides;

/// The `mlx_lm.server` default address, the same port as `llama-server`'s.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8080/v1";

/// Returns the builder of a provider for this server, with the user's declared capabilities
/// written over the data file's; the caller sets more on it (the wire recorder) before building.
pub(crate) fn mlx_builder(
    id: tars_types::ProviderId,
    base_url: Option<String>,
    auth: Auth,
    extras: HttpOverrides,
    capability_overrides: tars_config::CapabilitiesOverrides,
    model_catalog: &tars_config::ModelCatalog<'_>,
) -> OpenAiProviderBuilder {
    let url = base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let caps = capability_overrides.apply_to(model_catalog.capabilities_for("mlx", ""));
    let output_limits = tars_config::OutputLimitRule::catalog("mlx")
        .configured(capability_overrides.max_output_tokens);
    OpenAiProviderBuilder::new(id, normalize_auth(auth))
        .base_url(url)
        .extras(extras)
        .capabilities(caps)
        .output_limits(output_limits)
}

/// Returns `Auth::None` for an empty inline key, else `auth`: some gateways reject an empty
/// `Bearer ` header. A key of only spaces is not empty and is kept.
fn normalize_auth(auth: Auth) -> Auth {
    match auth {
        Auth::Secret {
            secret: tars_types::SecretRef::Inline { ref value },
        } if value.is_empty() => Auth::None,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::LlmProvider;
    use crate::provider::auth::basic;
    use crate::provider::http_base::HttpClient;
    use tars_types::ProviderId;

    fn built(auth: Auth) -> std::sync::Arc<dyn LlmProvider> {
        mlx_builder(
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
    async fn an_mlx_provider_has_the_data_files_profile() {
        let p = built(Auth::None);
        let caps = p.capabilities();
        assert!(matches!(
            caps.prompt_cache,
            tars_types::PromptCacheKind::None
        ));
        assert_eq!(caps.supports_vision, tars_types::Support::Unknown);
        assert_eq!(caps.supports_tool_use, tars_types::Support::Supported);
    }

    #[test]
    fn normalize_auth_coerces_empty_inline_to_none() {
        assert!(matches!(
            normalize_auth(Auth::inline(String::new())),
            Auth::None
        ));
    }

    #[test]
    fn normalize_auth_preserves_non_empty_inline() {
        match normalize_auth(Auth::inline("sk-test")) {
            Auth::Secret {
                secret: tars_types::SecretRef::Inline { value },
            } => {
                assert_eq!(value.expose(), "sk-test");
            }
            other => panic!("expected inline secret to be preserved, got {other:?}"),
        }
    }

    #[test]
    fn normalize_auth_preserves_none_and_delegate() {
        assert!(matches!(normalize_auth(Auth::None), Auth::None));
        assert!(matches!(normalize_auth(Auth::Delegate), Auth::Delegate));
    }

    #[tokio::test]
    async fn mlx_with_empty_inline_auth_constructs_provider() {
        let _ = built(Auth::inline(String::new()));
    }

    #[test]
    fn default_base_url_is_localhost_8080() {
        assert!(DEFAULT_BASE_URL.contains("8080"));
        assert!(DEFAULT_BASE_URL.ends_with("/v1"));
    }
}
