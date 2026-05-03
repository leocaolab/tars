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
use tars_config::Config;
use tars_pipeline::{Pipeline, RetryMiddleware, TelemetryMiddleware};
use tars_provider::auth::basic;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::registry::ProviderRegistry;
use tars_types::{
    ChatEvent, ChatRequest, CostUsd, ModelHint, ProviderId, RequestContext, Usage,
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

    /// Provider id to route through. Required iff config has > 1 provider.
    #[arg(short = 'P', long)]
    pub provider: Option<String>,

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
}

pub async fn execute(args: RunArgs, config_path: Option<PathBuf>) -> Result<()> {
    let cfg = config_loader::load(config_path)?;
    let registry = build_registry(&cfg)?;
    let provider_id = pick_provider(&cfg, args.provider.as_deref())?;
    let provider = registry.get(&provider_id).ok_or_else(|| {
        anyhow::anyhow!(
            "registry missing provider `{provider_id}` (validated config but build failed?)"
        )
    })?;

    let model_label = args
        .model
        .clone()
        .or_else(|| pick_default_model(&cfg, &provider_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no model: pass --model, or set `default_model` on provider `{provider_id}`"
            )
        })?;

    let req = build_request(&args, &model_label);
    // Capture the provider Arc for cost computation post-stream — the
    // Arc<dyn LlmProvider> is also moved into the pipeline below.
    let provider_for_cost = provider.clone();
    let pipeline = Pipeline::builder(provider)
        .layer(TelemetryMiddleware::new())
        .layer(RetryMiddleware::default())
        .build();

    let ctx = RequestContext::test_default(); // no IAM/audit yet (M6)
    let stream = Arc::new(pipeline)
        .call(req, ctx)
        .await
        .with_context(|| format!("opening stream against provider `{provider_id}`"))?;

    let outcome = drain_stream_to_stdout(stream).await?;

    if !args.no_summary {
        print_summary(&provider_for_cost, &outcome);
    }
    Ok(())
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
}

async fn drain_stream_to_stdout(
    mut stream: tars_provider::LlmEventStream,
) -> Result<StreamOutcome> {
    let mut outcome = StreamOutcome::default();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    while let Some(ev) = stream.next().await {
        match ev.context("stream error")? {
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

fn print_summary(provider: &Arc<dyn tars_provider::LlmProvider>, outcome: &StreamOutcome) {
    let cost = provider.cost(&outcome.usage);
    if outcome.wrote_anything {
        // Push the summary onto its own line so it doesn't glue to the response.
        let _ = writeln!(std::io::stdout());
    }
    let stop = outcome
        .stop_reason
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "<no Finished event>".into());
    eprintln!(
        "── tokens: in={} out={} thinking={} cached={}  cost: {}  stop: {stop}",
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
            model: None,
            max_output_tokens: Some(64),
            temperature: Some(0.3),
            no_summary: false,
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
