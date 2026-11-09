//! Builds the provider registry from a config, with or without a circuit breaker around each
//! provider, and checks or picks the provider a command routes through. The middleware chain
//! above the provider is each command's choice, not this module's.

use std::sync::Arc;

use tars_config::Config;
use tars_llm::provider::registry::{ProviderRegistry, RegistryError};
use tars_llm::{CircuitBreaker, CircuitBreakerConfig, HttpClientConfig};
use tars_types::ProviderId;

use crate::home_env::HomeEnv;

/// Says a provider a command named is not in the config, with the ids that are.
#[derive(Debug, thiserror::Error)]
#[error(
    "provider `{id}` is not configured. Configured: [{}]",
    join(configured)
)]
pub struct NotConfiguredError {
    id: ProviderId,
    configured: Vec<ProviderId>,
}

/// Why no provider was picked for a command that takes an optional `--provider`.
#[derive(Debug, thiserror::Error)]
pub enum PickProviderError {
    #[error(transparent)]
    NotConfigured(#[from] NotConfiguredError),
    #[error(
        "no providers declared in config; add a `[providers.NAME]` section, \
         or pass `--provider <BUILTIN_ID>` ({})",
        join(builtins)
    )]
    NoneDeclared { builtins: Vec<ProviderId> },
    #[error(
        "multiple providers declared ({}); pass --provider <ID>",
        join(declared)
    )]
    Ambiguous { declared: Vec<ProviderId> },
}

fn join(ids: &[ProviderId]) -> String {
    ids.iter()
        .map(ProviderId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the registry from `cfg`, its env-var keys and env headers read from `env`.
pub(crate) fn build_registry(
    cfg: &Config,
    env: Arc<HomeEnv>,
) -> Result<ProviderRegistry, RegistryError> {
    ProviderRegistry::from_config_with_env(
        cfg.providers(),
        tars_config::catalog(),
        http_config(),
        env,
        None,
    )
}

/// Returns the HTTP settings whose requests carry the call span's `traceparent`, so a gateway
/// that traces joins tars's trace.
#[cfg(feature = "otlp")]
fn http_config() -> HttpClientConfig {
    HttpClientConfig {
        trace_headers: Some(tars_telemetry::trace_headers),
        ..HttpClientConfig::default()
    }
}

/// Returns the default HTTP settings: without the `otlp` feature there is no trace to carry.
#[cfg(not(feature = "otlp"))]
fn http_config() -> HttpClientConfig {
    HttpClientConfig::default()
}

/// Builds the registry as [`build_registry`] does, every provider wrapped in a default
/// [`CircuitBreaker`].
pub(crate) fn build_registry_with_breakers(
    cfg: &Config,
    env: Arc<HomeEnv>,
) -> Result<Arc<ProviderRegistry>, RegistryError> {
    let breaker = CircuitBreakerConfig::default();
    let registry =
        build_registry(cfg, env)?.map_providers(|_id, p| CircuitBreaker::wrap(p, breaker));
    Ok(Arc::new(registry))
}

/// Returns `id` when the config has a provider of that id. A command checks this before it
/// builds the registry, so a misspelt name is not reported as another provider's build error.
///
/// # Errors
///
/// Fails when the config has none, naming the ids it has.
pub(crate) fn configured(cfg: &Config, id: ProviderId) -> Result<ProviderId, NotConfiguredError> {
    if cfg.providers().get(&id).is_some() {
        return Ok(id);
    }
    let mut configured: Vec<ProviderId> =
        cfg.providers().iter().map(|(id, _)| id.clone()).collect();
    configured.sort();
    Err(NotConfiguredError { id, configured })
}

/// Returns the provider `requested` names, else the config's one user-declared provider.
///
/// # Errors
///
/// Fails when `requested` is not in the config, or when nothing is requested and the config
/// declares no provider or more than one.
pub(crate) fn pick_provider(
    cfg: &Config,
    requested: Option<ProviderId>,
) -> Result<ProviderId, PickProviderError> {
    if let Some(id) = requested {
        return Ok(configured(cfg, id)?);
    }
    // Only user-declared providers count: the built-ins are merged into every config, so
    // counting them would make every config ambiguous.
    let mut declared: Vec<ProviderId> = cfg.user_declared().map(|(id, _)| id.clone()).collect();
    match declared.len() {
        0 => {
            let mut builtins: Vec<ProviderId> =
                cfg.providers().iter().map(|(id, _)| id.clone()).collect();
            builtins.sort();
            Err(PickProviderError::NoneDeclared { builtins })
        }
        1 => Ok(declared.remove(0)),
        _ => Err(PickProviderError::Ambiguous { declared }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(toml: &str) -> Config {
        Config::from_toml(toml).unwrap()
    }

    #[test]
    fn pick_provider_explicit_match() {
        let c = cfg(r#"
            [providers.foo]
            type = "mock"
            canned_response = "x"

            [providers.bar]
            type = "mock"
            canned_response = "y"
        "#);
        assert_eq!(
            pick_provider(&c, Some(ProviderId::new("bar")))
                .unwrap()
                .as_ref(),
            "bar"
        );
    }

    #[test]
    fn pick_provider_implicit_single_works() {
        let c = cfg(r#"
            [providers.only_one]
            type = "mock"
            canned_response = "x"
        "#);
        assert_eq!(pick_provider(&c, None).unwrap().as_ref(), "only_one");
    }

    #[test]
    fn pick_provider_implicit_ambiguous_errors() {
        let c = cfg(r#"
            [providers.a]
            type = "mock"
            canned_response = "x"

            [providers.b]
            type = "mock"
            canned_response = "y"
        "#);
        let err = pick_provider(&c, None).unwrap_err();
        assert!(err.to_string().contains("multiple"));
    }

    #[test]
    fn a_config_that_declares_no_provider_names_every_built_in() {
        let c = cfg("[providers]\n");
        match pick_provider(&c, None).unwrap_err() {
            PickProviderError::NoneDeclared { builtins } => {
                assert_eq!(builtins, tars_config::built_in_provider_ids());
            }
            other => panic!("expected NoneDeclared, got {other:?}"),
        }
    }
}
