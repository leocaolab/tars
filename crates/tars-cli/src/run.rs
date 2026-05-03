//! `tars run` — single-prompt streaming invocation.
//!
//! Doc 14 §7.2 acceptance script:
//!
//! ```text
//! tars run --prompt "Write a haiku about Rust"
//! ```
//!
//! Behaviour:
//! - Streams text deltas to stdout as they arrive (flushed per chunk).
//! - On stream end prints a one-line summary to stderr (so stdout
//!   stays pipeable): `tokens: <total>  cost: $<x.xxxx>`.
//! - Exits 0 on success; non-zero with chained error context on
//!   anything else.
//!
//! Provider selection rule (kept deliberately small for M1):
//! - If `--provider <ID>` is supplied → use it.
//! - Else if exactly one provider is configured → use it.
//! - Else → error listing the candidates.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args;
use futures::StreamExt;
use tars_cache::{
    open_at_path, CacheKeyFactory, CachePolicy, CacheRegistry, MemoryCacheRegistry,
};
use tars_config::Config;
use tars_pipeline::{
    set_cache_policy, CacheLookupMiddleware, LlmService, Pipeline, RetryMiddleware,
    RoutingService, StaticPolicy, TelemetryMiddleware,
};
use tars_provider::auth::basic;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::registry::ProviderRegistry;
use tars_types::{
    CacheHitInfo, ChatEvent, ChatRequest, CostUsd, ModelHint, ModelTier, ProviderId,
    RequestContext, Usage,
};

use crate::config_loader;

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Prompt to send. Reads stdin if `-`.
    #[arg(short, long)]
    pub prompt: String,

    /// Override the system prompt.
    #[arg(short, long)]
    pub system: Option<String>,

    /// Provider id to route through. Required iff config has > 1 provider
    /// AND `--tier` is not set. Mutually exclusive with `--tier`.
    #[arg(short = 'P', long, conflicts_with = "tier")]
    pub provider: Option<String>,

    /// Route by model tier instead of picking a single provider.
    /// Reads the `[routing.tiers]` section from config; tries each
    /// candidate in order with retriable-error fallback.
    /// Valid values: `reasoning`, `default`, `fast`, `local`.
    #[arg(short, long, conflicts_with = "provider", value_parser = parse_tier)]
    pub tier: Option<ModelTier>,

    /// Override the provider's `default_model`.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Maximum output tokens (provider default if omitted).
    #[arg(long)]
    pub max_output_tokens: Option<u32>,

    /// Sampling temperature. Provider default if omitted.
    #[arg(long)]
    pub temperature: Option<f32>,

    /// Skip the trailing `tokens: ... cost: ...` summary line.
    #[arg(long)]
    pub no_summary: bool,

    /// Disable response caching for this call. Useful when iterating
    /// on a prompt and you want to see the model's variation across
    /// requests (otherwise temperature=0 + cache returns the exact
    /// same bytes every time).
    #[arg(long)]
    pub no_cache: bool,

    /// Override the cache file path. Default: `$XDG_CACHE_HOME/tars/cache.sqlite`.
    /// Pass `:memory:` to use a per-invocation in-memory cache (useful
    /// when you want process-isolated caching for tests / scripted runs).
    #[arg(long, env = "TARS_CACHE_PATH")]
    pub cache_path: Option<PathBuf>,
}

pub async fn execute(args: RunArgs, config_path: Option<PathBuf>) -> Result<()> {
    let cfg = config_loader::load(config_path)?;
    let registry = Arc::new(build_registry(&cfg)?);

    // Decide the dispatch shape: single-provider passthrough vs
    // tier-routed multi-provider with fallback. Mutually exclusive
    // (clap's `conflicts_with` enforces it on the flag side).
    let dispatch = build_dispatch(&cfg, &registry, &args)?;

    let req = build_request(&args, &dispatch.model_label);

    // Cache: SQLite L2 by default (cross-invocation hits), in-memory
    // L1 always present. `--cache-path :memory:` falls back to pure-
    // in-memory; missing XDG cache dir does the same with a warning.
    let cache_registry = build_cache(args.cache_path.as_deref());
    let cache_factory = CacheKeyFactory::new(1);

    let pipeline = Pipeline::builder_with_inner(dispatch.inner)
        .layer(TelemetryMiddleware::new())
        .layer(CacheLookupMiddleware::new(
            cache_registry,
            cache_factory,
            dispatch.cache_origin_id.clone(),
        ))
        .layer(RetryMiddleware::default())
        .build();

    let ctx = RequestContext::test_default(); // no IAM/audit yet (M6)
    if args.no_cache {
        set_cache_policy(&ctx, &CachePolicy::off());
    }
    let dispatch_label = dispatch.label.clone();
    let stream = Arc::new(pipeline)
        .call(req, ctx)
        .await
        .with_context(|| format!("opening stream via {dispatch_label}"))?;

    let outcome = drain_stream_to_stdout(stream).await?;

    if !args.no_summary {
        print_summary(dispatch.cost_provider.as_ref(), &outcome);
    }
    Ok(())
}

/// What `execute` needs to drive the pipeline once per call: the
/// bottom-of-pipeline service, plus diagnostic / billing-attribution
/// metadata.
struct Dispatch {
    inner: Arc<dyn LlmService>,
    /// The model to put on `req.model` before sending. For single-
    /// provider mode it's `--model` or the provider's `default_model`.
    /// For tier mode it's the FIRST candidate's `default_model`
    /// (a hint; the routing layer resolves Tier→Explicit per call,
    /// see `RoutingService::resolve_model_for_provider`).
    model_label: String,
    /// What to log / attribute cost against. For single-provider mode
    /// this is the actual provider; for tier mode it's the first
    /// candidate (best-effort — until we surface "which provider
    /// actually answered" through the stream, this is the closest
    /// approximation).
    cost_provider: Arc<dyn tars_provider::LlmProvider>,
    /// ProviderId stamped on cached responses' `origin_provider` field.
    /// For single-provider mode = the provider. For tier mode = the
    /// first candidate (same caveat as `cost_provider`).
    cache_origin_id: ProviderId,
    /// Diagnostic label for log + error context.
    label: String,
}

fn build_dispatch(
    cfg: &Config,
    registry: &Arc<ProviderRegistry>,
    args: &RunArgs,
) -> Result<Dispatch> {
    if let Some(tier) = args.tier {
        return build_tier_dispatch(cfg, registry, tier, args);
    }
    build_single_provider_dispatch(cfg, registry, args)
}

fn build_single_provider_dispatch(
    cfg: &Config,
    registry: &Arc<ProviderRegistry>,
    args: &RunArgs,
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
    let label = format!("provider `{provider_id}`");
    let inner: Arc<dyn LlmService> = tars_pipeline::ProviderService::new(provider.clone());
    Ok(Dispatch {
        inner,
        model_label,
        cost_provider: provider,
        cache_origin_id: provider_id,
        label,
    })
}

fn build_tier_dispatch(
    cfg: &Config,
    registry: &Arc<ProviderRegistry>,
    tier: ModelTier,
    args: &RunArgs,
) -> Result<Dispatch> {
    let candidates = cfg.routing.tiers.get(&tier).cloned().unwrap_or_default();
    if candidates.is_empty() {
        anyhow::bail!(
            "routing: tier `{tier:?}` has no candidates configured. \
             Add `[routing.tiers]\\n{} = [\\\"...\\\"]` to your config.",
            format!("{tier:?}").to_lowercase(),
        );
    }
    // The first candidate becomes our cost / cache-attribution proxy.
    // It must exist in the registry (validated at config-load time,
    // but defensive double-check).
    let first = candidates.first().expect("non-empty checked above");
    let cost_provider = registry.get(first).ok_or_else(|| {
        anyhow::anyhow!(
            "routing: tier `{tier:?}` first candidate `{first}` not in registry"
        )
    })?;
    let model_label = args
        .model
        .clone()
        .or_else(|| pick_default_model(cfg, first))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no model: pass --model, or set `default_model` on provider `{first}` (tier `{tier:?}` first candidate)"
            )
        })?;

    // Tier→candidates resolution happens here at startup; the runtime
    // policy is StaticPolicy with the resolved list. We don't use
    // TierPolicy at runtime because that would require req.model to be
    // ModelHint::Tier(...) for the lookup to fire — but the CLI's
    // existing flow always sets req.model = Explicit (from --model or
    // the provider's default_model). Resolving up-front keeps the two
    // concerns clean: config layer maps tiers, runtime layer just
    // dispatches in fallback order.
    let policy = Arc::new(StaticPolicy::new(candidates.clone()));
    let inner: Arc<dyn LlmService> = RoutingService::new(registry.clone(), policy);
    let label = format!(
        "tier `{tier:?}` (candidates: {})",
        candidates
            .iter()
            .map(|p| p.as_ref())
            .collect::<Vec<_>>()
            .join(", "),
    );
    Ok(Dispatch {
        inner,
        model_label,
        cost_provider,
        cache_origin_id: first.clone(),
        label,
    })
}

/// Clap value parser for `--tier` — explicit set of accepted values.
fn parse_tier(s: &str) -> Result<ModelTier, String> {
    match s.to_ascii_lowercase().as_str() {
        "reasoning" => Ok(ModelTier::Reasoning),
        "default" => Ok(ModelTier::Default),
        "fast" => Ok(ModelTier::Fast),
        "local" => Ok(ModelTier::Local),
        _ => Err(format!(
            "unknown tier `{s}` (valid: reasoning, default, fast, local)"
        )),
    }
}

/// Pick a cache backend based on the `--cache-path` flag and platform:
/// - explicit `:memory:` → in-memory L1 only
/// - explicit path → SQLite at that path (parents created as needed)
/// - default → SQLite at `dirs::cache_dir()/tars/cache.sqlite`
/// - no XDG cache dir on this platform → in-memory L1 with a warning
///
/// Falls back to in-memory on any sqlite open / migration failure too —
/// caching is best-effort, never fatal (Doc 03 §4.3).
fn build_cache(explicit: Option<&std::path::Path>) -> Arc<dyn CacheRegistry> {
    use std::path::Path;

    fn warn_to_memory(reason: &str) -> Arc<dyn CacheRegistry> {
        tracing::warn!("cache: {reason}; using in-memory L1 only (no cross-process hits)");
        MemoryCacheRegistry::default_arc()
    }

    let path = match explicit {
        Some(p) if p == Path::new(":memory:") => return MemoryCacheRegistry::default_arc(),
        Some(p) => p.to_path_buf(),
        None => match tars_cache::default_personal_cache_path() {
            Some(p) => p,
            None => return warn_to_memory("no XDG cache dir on this platform"),
        },
    };

    match open_at_path(&path) {
        Ok(reg) => reg,
        Err(e) => warn_to_memory(&format!("opening sqlite cache at {path:?} failed: {e}")),
    }
}

fn build_registry(cfg: &Config) -> Result<ProviderRegistry> {
    let http = HttpProviderBase::default_arc().context("constructing reqwest client")?;
    ProviderRegistry::from_config(&cfg.providers, http, basic())
        .context("building provider registry from config")
}

fn pick_provider(cfg: &Config, requested: Option<&str>) -> Result<ProviderId> {
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
    let mut iter = cfg.providers.iter();
    let only = iter.next();
    let extras = iter.next();
    match (only, extras) {
        (Some((id, _)), None) => Ok(id.clone()),
        (None, _) => anyhow::bail!("no providers configured"),
        (Some(_), Some(_)) => {
            let configured: Vec<String> =
                cfg.providers.iter().map(|(id, _)| id.to_string()).collect();
            anyhow::bail!(
                "multiple providers configured ({}); pass --provider <ID>",
                configured.join(", "),
            );
        }
    }
}

fn pick_default_model(cfg: &Config, provider_id: &ProviderId) -> Option<String> {
    cfg.providers.get(provider_id).map(|p| p.default_model().to_string())
}

fn build_request(args: &RunArgs, model: &str) -> ChatRequest {
    let mut req = ChatRequest::user(ModelHint::Explicit(model.to_string()), &args.prompt);
    if let Some(s) = &args.system {
        req = req.with_system(s);
    }
    req.max_output_tokens = args.max_output_tokens;
    req.temperature = args.temperature;
    req
}

/// What we collected by the time the stream ended.
#[derive(Debug, Default)]
pub struct StreamOutcome {
    pub usage: Usage,
    pub stop_reason: Option<tars_types::StopReason>,
    /// True if we ever wrote *something* to stdout. Lets us print
    /// a leading newline before the summary only when needed.
    pub wrote_anything: bool,
    /// Cache info from the Started event. Non-zero
    /// `cached_input_tokens` means we replayed a hit.
    pub cache_hit: CacheHitInfo,
}

async fn drain_stream_to_stdout(
    mut stream: tars_provider::LlmEventStream,
) -> Result<StreamOutcome> {
    let mut outcome = StreamOutcome::default();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    while let Some(ev) = stream.next().await {
        match ev.context("stream error")? {
            ChatEvent::Started { cache_hit, .. } => {
                outcome.cache_hit = cache_hit;
            }
            ChatEvent::Delta { text } => {
                out.write_all(text.as_bytes()).context("stdout write")?;
                out.flush().context("stdout flush")?;
                outcome.wrote_anything = !text.is_empty() || outcome.wrote_anything;
            }
            ChatEvent::ThinkingDelta { .. } => {
                // Hide thinking deltas from stdout by default — they're
                // diagnostic, not response. Could add a --show-thinking flag.
            }
            ChatEvent::Finished { stop_reason, usage } => {
                outcome.stop_reason = Some(stop_reason);
                outcome.usage = usage;
            }
            _ => {}
        }
    }
    Ok(outcome)
}

fn print_summary(provider: &dyn tars_provider::LlmProvider, outcome: &StreamOutcome) {
    let cost = provider.cost(&outcome.usage);
    if outcome.wrote_anything {
        // Push the summary onto its own line so it doesn't glue to the response.
        let _ = writeln!(std::io::stdout());
    }
    let stop = outcome
        .stop_reason
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "<no Finished event>".into());
    let cache_tag = if outcome.cache_hit.replayed_from_cache {
        " (cache hit; cost saved)"
    } else {
        ""
    };
    eprintln!(
        "── tokens: in={} out={} thinking={} cached={}  cost: {}{cache_tag}  stop: {stop}",
        outcome.usage.input_tokens,
        outcome.usage.output_tokens,
        outcome.usage.thinking_tokens,
        outcome.usage.cached_input_tokens,
        format_cost(cost),
    );
}

fn format_cost(cost: CostUsd) -> String {
    let v = cost.as_f64();
    if v == 0.0 {
        "$0 (free)".into()
    } else if v < 0.0001 {
        format!("${v:.6}")
    } else {
        format!("${v:.4}")
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
        let c = cfg(
            r#"
            [providers.foo]
            type = "mock"
            canned_response = "x"

            [providers.bar]
            type = "mock"
            canned_response = "y"
            "#,
        );
        let p = pick_provider(&c, Some("bar")).unwrap();
        assert_eq!(p.as_ref(), "bar");
    }

    #[test]
    fn pick_provider_explicit_unknown_lists_candidates() {
        let c = cfg(
            r#"
            [providers.foo]
            type = "mock"
            canned_response = "x"
            "#,
        );
        let err = pick_provider(&c, Some("nope")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("`nope`"));
        assert!(msg.contains("foo"), "should list configured: got {msg:?}");
    }

    #[test]
    fn pick_provider_implicit_single_works() {
        let c = cfg(
            r#"
            [providers.only_one]
            type = "mock"
            canned_response = "x"
            "#,
        );
        assert_eq!(pick_provider(&c, None).unwrap().as_ref(), "only_one");
    }

    #[test]
    fn pick_provider_implicit_ambiguous_errors() {
        let c = cfg(
            r#"
            [providers.a]
            type = "mock"
            canned_response = "x"

            [providers.b]
            type = "mock"
            canned_response = "y"
            "#,
        );
        let err = pick_provider(&c, None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("multiple"));
        assert!(msg.contains("--provider"));
    }

    #[test]
    fn build_request_propagates_overrides() {
        let args = RunArgs {
            prompt: "hi".into(),
            system: Some("be brief".into()),
            provider: None,
            tier: None,
            model: None,
            max_output_tokens: Some(64),
            temperature: Some(0.3),
            no_summary: false,
            no_cache: false,
            cache_path: None,
        };
        let req = build_request(&args, "gpt-4o");
        assert_eq!(req.max_output_tokens, Some(64));
        assert_eq!(req.temperature, Some(0.3));
        assert_eq!(req.system.as_deref(), Some("be brief"));
        assert!(matches!(req.model, ModelHint::Explicit(ref m) if m == "gpt-4o"));
    }

    #[test]
    fn format_cost_chooses_precision_by_magnitude() {
        assert_eq!(format_cost(CostUsd(0.0)), "$0 (free)");
        assert_eq!(format_cost(CostUsd(0.000_012)), "$0.000012");
        assert_eq!(format_cost(CostUsd(0.0123)), "$0.0123");
    }
}
