//! Builds an OpenAI provider for a vLLM server, by default `http://localhost:8000/v1` with no
//! key. It has no adapter of its own.

use crate::provider::auth::Auth;
use crate::provider::backends::openai::OpenAiProviderBuilder;
use crate::provider::http_base::HttpOverrides;

pub const DEFAULT_BASE_URL: &str = "http://localhost:8000/v1";

/// Returns `base_url`, or the default when it is absent or blank: a blank URL is a config slip,
/// and a refused localhost connection is easier to debug than a malformed URL.
pub(crate) fn normalize_base_url(base_url: Option<String>) -> String {
    base_url
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// Returns `Auth::None` for a blank inline key, else `auth`. vLLM checks no key by default, and
/// an empty `Bearer ` header is rejected by some gateways with unclear errors.
fn normalize_auth(auth: Auth) -> Auth {
    match auth {
        Auth::Secret {
            secret: tars_types::SecretRef::Inline { ref value },
        } if value.expose().trim().is_empty() => Auth::None,
        other => other,
    }
}

/// Returns the builder of a provider for this server, with the user's declared capabilities
/// written over the data file's; the caller sets more on it (the wire recorder) before building.
pub(crate) fn vllm_builder(
    id: tars_types::ProviderId,
    base_url: Option<String>,
    auth: Auth,
    extras: HttpOverrides,
    capability_overrides: tars_config::CapabilitiesOverrides,
    model_catalog: &tars_config::ModelCatalog<'_>,
) -> OpenAiProviderBuilder {
    let url = normalize_base_url(base_url);
    let normalized_auth = normalize_auth(auth);
    let caps = capability_overrides.apply_to(model_catalog.capabilities_for("vllm", ""));
    let output_limits = tars_config::OutputLimitRule::catalog("vllm")
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
    use tars_types::SecretRef;

    fn built(auth: Auth) -> std::sync::Arc<dyn LlmProvider> {
        vllm_builder(
            ProviderId::new("t"),
            None,
            auth,
            HttpOverrides::default(),
            tars_config::CapabilitiesOverrides::default(),
            tars_config::catalog(),
        )
        .build(HttpClient::default_arc().unwrap(), basic())
    }

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
        let weird = "not a url".to_string();
        assert_eq!(normalize_base_url(Some(weird.clone())), weird);
    }

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
            Auth::Secret {
                secret: SecretRef::Inline { value },
            } => {
                assert_eq!(value.expose(), "sk-real-key");
            }
            other => panic!("expected inline secret, got {other:?}"),
        }
    }

    #[test]
    fn auth_env_variant_passes_through() {
        let out = normalize_auth(Auth::env("VLLM_KEY"));
        match out {
            Auth::Secret {
                secret: SecretRef::Env { var },
            } => {
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

    #[tokio::test]
    async fn a_vllm_provider_has_the_data_files_profile() {
        let p = built(Auth::None);
        let caps = p.capabilities();
        assert!(matches!(
            caps.prompt_cache,
            tars_types::PromptCacheKind::None
        ));
        assert_eq!(caps.supports_thinking, tars_types::Support::Unknown);
        assert_eq!(caps.supports_tool_use, tars_types::Support::Supported);
    }

    #[tokio::test]
    async fn vllm_accepts_empty_inline_auth() {
        let _ = built(Auth::inline(String::new()));
    }
}
