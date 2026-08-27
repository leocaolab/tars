//! Shared dispatch wiring used by every subcommand that talks to an
//! LLM (`tars run`, `tars plan`, future `tars chat`).
//!
//! Three responsibilities:
//!
//! 1. **Common flags** as the [`DispatchArgs`] struct that
//!    subcommands `#[command(flatten)]` into their own arg structs —
//!    keeps `--provider / --tier / --model / --cache-path / --breaker
//!    / --events-path / --no-cache / --no-trajectory` semantics
//!    identical across subcommands.
//! 2. **Provider dispatch** — turn config + flags into a
//!    [`Dispatch`] struct (the bottom-of-pipeline `LlmService` plus
//!    the bookkeeping every caller needs: model label for
//!    `req.model`, cost-attribution provider, cache origin id,
//!    diagnostic label).
//! 3. **Cache + registry construction** — same fallback logic
//!    (XDG default → SQLite → in-memory on failure) every subcommand
//!    needs.
//!
//! The actual pipeline composition (which middleware layers in which
//! order) stays per-subcommand because subcommands have legitimate
//! reasons to differ — e.g., a future `tars chat` will want
//! conversation-context middleware that `tars run` doesn't.

use std::sync::Arc;

use anyhow::{Context, Result};
use tars_config::Config;
use tars_pipeline::{CircuitBreaker, CircuitBreakerConfig};
use tars_provider::registry::ProviderRegistry;
use tars_types::ProviderId;





/// Build the registry, optionally wrapping providers with CircuitBreaker.
pub fn build_registry_with_breaker(
    cfg: &Config,
    breaker_enabled: bool,
) -> Result<Arc<ProviderRegistry>> {
    let mut registry = build_registry(cfg)?;
    if breaker_enabled {
        let cfg_default = CircuitBreakerConfig::default();
        registry = registry.map_providers(|_id, p| CircuitBreaker::wrap(p, cfg_default.clone()));
    }
    Ok(Arc::new(registry))
}

fn build_registry(cfg: &Config) -> Result<ProviderRegistry> {
    ProviderRegistry::from_config_default(&cfg.providers)
        .context("building provider registry from config")
}

pub fn pick_provider(cfg: &Config, requested: Option<&str>) -> Result<ProviderId> {
    if let Some(id) = requested {
        let pid = ProviderId::new(id);
        if cfg.providers.get(&pid).is_none() {
            let configured: Vec<String> =
                cfg.providers.iter().map(|(id, _)| id.to_string()).collect();
            anyhow::bail!(
                "provider `{id}` not in config. Configured: [{}]",
                configured.join(", ")
            );
        }
        return Ok(pid);
    }
    // Implicit pick considers user-declared providers only — ambient
    // builtins are always present after the load-time merge, so
    // counting them would make every config "ambiguous". The user's
    // mental model is "I wrote one provider in my TOML, use it."
    let mut iter = cfg.user_declared();
    let only = iter.next();
    let extras = iter.next();
    match (only, extras) {
        (Some((id, _)), None) => Ok(id.clone()),
        (None, _) => anyhow::bail!(
            "no providers declared in config; add a `[providers.NAME]` section, \
             or pass `--provider <BUILTIN_ID>` (mlx / vllm / openai / anthropic / \
             gemini / claude_cli / gemini_cli / llamacpp)"
        ),
        (Some(_), Some(_)) => {
            let configured: Vec<String> =
                cfg.user_declared().map(|(id, _)| id.to_string()).collect();
            anyhow::bail!(
                "multiple providers declared ({}); pass --provider <ID>",
                configured.join(", "),
            );
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use tars_config::ConfigManager;

    fn cfg(toml: &str) -> Config {
        ConfigManager::load_from_str(toml).unwrap()
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
        assert_eq!(pick_provider(&c, Some("bar")).unwrap().as_ref(), "bar");
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

}
