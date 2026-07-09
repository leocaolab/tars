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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args;
use tars_cache::{CacheRegistry, MemoryCacheRegistry, open_at_path};
use tars_config::Config;
use tars_pipeline::{CircuitBreaker, CircuitBreakerConfig, LlmService};
use tars_provider::registry::ProviderRegistry;
use tars_types::ProviderId;

/// Flags every LLM-calling subcommand shares. Flatten with
/// `#[command(flatten)] pub dispatch: DispatchArgs` on each
/// subcommand's args struct.
#[derive(Args, Debug, Clone)]
pub struct DispatchArgs {
    /// Provider id to route through. Required iff config has > 1
    /// provider.
    #[arg(short = 'P', long)]
    pub provider: Option<String>,

    /// Override the provider's `default_model`.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Disable response caching for this call.
    #[arg(long)]
    pub no_cache: bool,

    /// Override the cache file path. Default: `$XDG_CACHE_HOME/tars/cache.sqlite`.
    /// Pass `:memory:` to use a per-invocation in-memory cache.
    #[arg(long, env = "TARS_CACHE_PATH")]
    pub cache_path: Option<PathBuf>,

    /// Wrap each registry provider in a `CircuitBreaker` before routing.
    /// Cross-call value lives in long-lived consumers (`tars chat`,
    /// future server); a single CLI invocation gets little benefit.
    #[arg(long)]
    pub breaker: bool,

    /// Skip writing this invocation to the trajectory event store.
    #[arg(long)]
    pub no_trajectory: bool,

    /// Override the event store path. Default:
    /// `$XDG_DATA_HOME/tars/events.sqlite`.
    #[arg(long, env = "TARS_EVENTS_PATH")]
    pub events_path: Option<PathBuf>,
}

/// What every subcommand needs to drive the pipeline once per call.
pub struct Dispatch {
    /// Bottom-of-pipeline service, bound to `model_label`. Subcommands
    /// wrap this with their own middleware stack (Telemetry /
    /// CacheLookup / Retry / etc.).
    pub inner: LlmService,
    /// The concrete model the service is bound to. Resolved from
    /// `--model` or the chosen provider's `default_model`.
    pub model_label: String,
    /// What to attribute cost against — the single dispatched provider.
    pub cost_provider: Arc<dyn tars_provider::LlmProvider>,
    /// `ProviderId` stamped on cached responses' `origin_provider`.
    pub cache_origin_id: ProviderId,
    /// Human-readable label for log + error context.
    pub label: String,
}

/// Decide the dispatch shape from config + flags. Single-provider only:
/// provider selection (routing / tier / ensemble) is not a pipeline
/// concern — a caller who wants it composes several `LlmService`s.
pub fn build_dispatch(
    cfg: &Config,
    registry: &Arc<ProviderRegistry>,
    args: &DispatchArgs,
) -> Result<Dispatch> {
    build_single_provider_dispatch(cfg, registry, args)
}

fn build_single_provider_dispatch(
    cfg: &Config,
    registry: &Arc<ProviderRegistry>,
    args: &DispatchArgs,
) -> Result<Dispatch> {
    let provider_id = pick_provider(cfg, args.provider.as_deref())?;
    let provider = registry.get(&provider_id).ok_or_else(|| {
        anyhow::anyhow!(
            "registry missing provider `{provider_id}` (validated config but build failed?)"
        )
    })?;
    let model_label = args
        .model
        .clone()
        .or_else(|| pick_default_model(cfg, &provider_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no model: pass --model, or set `default_model` on provider `{provider_id}`"
            )
        })?;
    if model_label.trim().is_empty() {
        anyhow::bail!(
            "model name is empty (from --model or provider `{provider_id}`'s `default_model`); \
             pass a non-empty --model"
        );
    }
    let label = format!("provider `{provider_id}`");
    let inner = LlmService::of(provider.clone(), model_label.clone());
    Ok(Dispatch {
        inner,
        model_label,
        cost_provider: provider,
        cache_origin_id: provider_id,
        label,
    })
}

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

pub fn pick_default_model(cfg: &Config, provider_id: &ProviderId) -> Option<String> {
    cfg.providers
        .get(provider_id)
        .map(|p| p.default_model().to_string())
}

/// Build the cache backend per the `--cache-path` flag and platform.
/// SQLite at the resolved path by default; `:memory:` sentinel for
/// process-isolated tests. Default-path failures (no XDG dir, sqlite
/// open error) degrade to in-memory L1 with a warning. Explicit-path
/// failures error out — silently demoting a user's `--cache-path` to
/// in-memory hides a config or permissions bug.
pub fn build_cache(explicit: Option<&Path>) -> Result<Arc<dyn CacheRegistry>> {
    fn warn_to_memory(reason: &str) -> Arc<dyn CacheRegistry> {
        tracing::warn!("cache: {reason}; using in-memory L1 only (no cross-process hits)");
        MemoryCacheRegistry::default_arc()
    }

    let (path, is_explicit) = match explicit {
        Some(p) if p == Path::new(":memory:") => return Ok(MemoryCacheRegistry::default_arc()),
        Some(p) => (p.to_path_buf(), true),
        None => match tars_cache::default_personal_cache_path() {
            Some(p) => (p, false),
            None => return Ok(warn_to_memory("no XDG cache dir on this platform")),
        },
    };
    match open_at_path(&path) {
        Ok(reg) => Ok(reg),
        Err(e) if is_explicit => Err(e)
            .with_context(|| format!("opening sqlite cache at explicit --cache-path {path:?}")),
        Err(e) => Ok(warn_to_memory(&format!(
            "opening sqlite cache at {path:?} failed: {e}"
        ))),
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
