//! Map [`tars_config::ProvidersConfig`] → live `Arc<dyn LlmProvider>` instances.
//!
//! This is the bridge between the declarative config layer and the
//! runtime provider builders. It's intentionally a flat factory
//! (one match arm per [`ProviderConfig`] variant) — easier to reason
//! about than a registration / IoC scheme, at the cost of needing to
//! grow the match arm when a new provider lands.
//!
//! The registry holds the providers it built so callers (Routing,
//! Pipeline, tests) can look them up by id.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use tars_config::{ProviderConfig, ProvidersConfig};
use tars_types::{Auth, ProviderId};

use crate::auth::AuthResolver;
use crate::backends::anthropic::AnthropicProviderBuilder;
use crate::backends::claude_cli::ClaudeCliProviderBuilder;
use crate::backends::gemini::GeminiProviderBuilder;
use crate::backends::gemini_cli::GeminiCliProviderBuilder;
use crate::backends::mock::{CannedResponse, MockProvider};
use crate::backends::openai::OpenAiProviderBuilder;
use crate::backends::llamacpp::llamacpp;
use crate::backends::mlx::mlx;
use crate::backends::vllm::vllm;
use crate::http_base::HttpProviderBase;
use crate::provider::LlmProvider;

#[derive(Debug, Error)]
pub enum RegistryError {
    /// `id` already exists in the registry — config has duplicate keys.
    /// (Shouldn't happen because TOML rejects duplicates, but defensive.)
    #[error("duplicate provider id: {0}")]
    Duplicate(ProviderId),
    /// Failure constructing the underlying HTTP base.
    #[error("http base init: {0}")]
    HttpBaseInit(String),
}

/// Built map of providers, indexed by id. Cheap to clone (everything is Arc).
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: Arc<HashMap<ProviderId, Arc<dyn LlmProvider>>>,
}

impl ProviderRegistry {
    /// Empty registry — useful in tests that build providers manually.
    pub fn empty() -> Self {
        Self { providers: Arc::new(HashMap::new()) }
    }

    /// Build all providers declared in `cfg`. The shared HTTP base + auth
    /// resolver are passed once and reused across HTTP-backed providers.
    pub fn from_config(
        cfg: &ProvidersConfig,
        http: Arc<HttpProviderBase>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Result<Self, RegistryError> {
        let mut map: HashMap<ProviderId, Arc<dyn LlmProvider>> = HashMap::new();
        for (id, entry) in cfg.iter() {
            let provider =
                build_one(id.clone(), entry, http.clone(), auth_resolver.clone())?;
            if map.insert(id.clone(), provider).is_some() {
                return Err(RegistryError::Duplicate(id.clone()));
            }
        }
        Ok(Self { providers: Arc::new(map) })
    }

    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn LlmProvider>> {
        self.providers.get(id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &ProviderId> {
        self.providers.keys()
    }

    pub fn len(&self) -> usize {
        self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// Build a single provider from its [`ProviderConfig`] variant.
///
/// Returned as `Arc<dyn LlmProvider>` so the caller can put concrete
/// types (`Arc<OpenAiProvider>`, `Arc<AnthropicProvider>`, …) into a
/// uniform map.
fn build_one(
    id: ProviderId,
    cfg: &ProviderConfig,
    http: Arc<HttpProviderBase>,
    auth_resolver: Arc<dyn AuthResolver>,
) -> Result<Arc<dyn LlmProvider>, RegistryError> {
    let provider: Arc<dyn LlmProvider> = match cfg {
        ProviderConfig::Openai {
            base_url,
            auth,
            default_model: _,
            extras,
        } => {
            let mut builder =
                OpenAiProviderBuilder::new(id, auth.clone()).extras(extras.clone());
            if let Some(url) = base_url {
                builder = builder.base_url(url.clone());
            }
            builder.build(http, auth_resolver)
        }

        ProviderConfig::OpenaiCompat {
            base_url,
            auth,
            default_model: _,
            extras,
        } => OpenAiProviderBuilder::new(id, auth.clone())
            .base_url(base_url.clone())
            .extras(extras.clone())
            .build(http, auth_resolver),

        ProviderConfig::Anthropic {
            base_url,
            api_version,
            auth,
            default_model: _,
            extras,
        } => {
            let mut builder =
                AnthropicProviderBuilder::new(id, auth.clone()).extras(extras.clone());
            if let Some(url) = base_url {
                builder = builder.base_url(url.clone());
            }
            if let Some(v) = api_version {
                builder = builder.api_version(v.clone());
            }
            builder.build(http, auth_resolver)
        }

        ProviderConfig::Gemini {
            base_url,
            auth,
            default_model: _,
            extras,
        } => {
            let mut builder =
                GeminiProviderBuilder::new(id, auth.clone()).extras(extras.clone());
            if let Some(url) = base_url {
                builder = builder.base_url(url.clone());
            }
            builder.build(http, auth_resolver)
        }

        ProviderConfig::Vllm {
            base_url,
            auth,
            default_model: _,
            extras,
        } => vllm(
            id,
            base_url.clone(),
            auth.clone(),
            extras.clone(),
            http,
            auth_resolver,
        ),

        ProviderConfig::Mlx {
            base_url,
            auth,
            default_model: _,
            extras,
        } => mlx(
            id,
            base_url.clone(),
            auth.clone(),
            extras.clone(),
            http,
            auth_resolver,
        ),

        ProviderConfig::Llamacpp {
            base_url,
            auth,
            default_model: _,
            extras,
        } => llamacpp(
            id,
            base_url.clone(),
            auth.clone(),
            extras.clone(),
            http,
            auth_resolver,
        ),

        ProviderConfig::ClaudeCli { executable, timeout_secs, default_model: _ } => {
            ClaudeCliProviderBuilder::new(id)
                .executable(executable.clone())
                .timeout(Duration::from_secs(*timeout_secs))
                .build()
        }

        ProviderConfig::GeminiCli { executable, timeout_secs, default_model: _ } => {
            GeminiCliProviderBuilder::new(id)
                .executable(executable.clone())
                .timeout(Duration::from_secs(*timeout_secs))
                .build()
        }

        ProviderConfig::Mock { canned_response } => {
            MockProvider::new(id, CannedResponse::Text(canned_response.clone()))
        }
    };

    // Suppress unused-Auth-Variant warnings for now while CLI providers
    // don't consume Auth (they use Delegate semantics implicitly).
    let _ = Auth::None;
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::basic;
    use tars_config::ConfigManager;

    fn http() -> Arc<HttpProviderBase> {
        HttpProviderBase::default_arc().unwrap()
    }

    #[test]
    fn empty_registry_is_empty() {
        let r = ProviderRegistry::empty();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn builds_each_supported_variant() {
        let toml_str = r#"
            [providers.openai_main]
            type = "openai"
            auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
            default_model = "gpt-4o"

            [providers.openai_compat_local]
            type = "openai_compat"
            base_url = "http://localhost:8000/v1"
            default_model = "Qwen/Qwen2.5-Coder-7B-Instruct"

            [providers.anthropic_main]
            type = "anthropic"
            auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY" } }
            default_model = "claude-opus-4-7"

            [providers.gemini_main]
            type = "gemini"
            auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
            default_model = "gemini-2.5-pro"

            [providers.vllm_local]
            type = "vllm"
            default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"

            [providers.mlx_local]
            type = "mlx"
            default_model = "mlx-community/Qwen2.5-Coder-32B-Instruct-4bit"

            [providers.llamacpp_local]
            type = "llamacpp"
            default_model = "Qwen2.5-Coder-7B-Q5_K_M"

            [providers.claude_cli]
            type = "claude_cli"
            default_model = "claude-opus-4-7"

            [providers.gemini_cli]
            type = "gemini_cli"
            default_model = "gemini-2.5-pro"

            [providers.mock_test]
            type = "mock"
            canned_response = "hi"
        "#;
        let cfg = ConfigManager::load_from_str(toml_str).unwrap();
        let reg = ProviderRegistry::from_config(&cfg.providers, http(), basic()).unwrap();
        assert_eq!(reg.len(), 10);
        assert!(reg.get(&ProviderId::new("openai_main")).is_some());
        assert!(reg.get(&ProviderId::new("openai_compat_local")).is_some());
        assert!(reg.get(&ProviderId::new("anthropic_main")).is_some());
        assert!(reg.get(&ProviderId::new("gemini_main")).is_some());
        assert!(reg.get(&ProviderId::new("vllm_local")).is_some());
        assert!(reg.get(&ProviderId::new("mlx_local")).is_some());
        assert!(reg.get(&ProviderId::new("llamacpp_local")).is_some());
        assert!(reg.get(&ProviderId::new("claude_cli")).is_some());
        assert!(reg.get(&ProviderId::new("gemini_cli")).is_some());
        assert!(reg.get(&ProviderId::new("mock_test")).is_some());
    }

    #[tokio::test]
    async fn mock_provider_built_from_config_actually_responds() {
        use tars_types::{ChatRequest, ModelHint, RequestContext};

        let cfg = ConfigManager::load_from_str(
            r#"
            [providers.greeter]
            type = "mock"
            canned_response = "hello from config"
        "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config(&cfg.providers, http(), basic()).unwrap();
        let provider = reg.get(&ProviderId::new("greeter")).unwrap();
        let resp = provider
            .complete(
                ChatRequest::user(ModelHint::Explicit("mock-1".into()), "ping"),
                RequestContext::test_default(),
            )
            .await
            .unwrap();
        assert_eq!(resp.text, "hello from config");
    }

    #[test]
    fn unknown_provider_id_returns_none() {
        let cfg = ConfigManager::load_from_str(
            r#"
            [providers.x]
            type = "mock"
            canned_response = "ok"
        "#,
        )
        .unwrap();
        let reg = ProviderRegistry::from_config(&cfg.providers, http(), basic()).unwrap();
        assert!(reg.get(&ProviderId::new("does_not_exist")).is_none());
    }
}
