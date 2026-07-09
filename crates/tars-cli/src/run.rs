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
use tars_cache::{CacheKeyFactory, CachePolicy};
use tars_pipeline::{
    CacheLookupMiddleware, LlmService, RetryMiddleware, TelemetryMiddleware, set_cache_policy,
};
use tars_runtime::{AgentEvent, LocalRuntime, Runtime, StepIdempotencyKey};
use tars_types::{
    CacheHitInfo, ChatEvent, ChatRequest, CostUsd, RequestContext, TrajectoryId, Usage,
};

use crate::dispatch::{
    Dispatch, DispatchArgs, build_cache, build_dispatch, build_registry_with_breaker,
};
use crate::{config_loader, event_store};

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Common dispatch flags (provider/tier/model/cache/breaker/trajectory).
    #[command(flatten)]
    pub dispatch: DispatchArgs,

    /// Prompt to send. Reads stdin if `-`.
    #[arg(short, long)]
    pub prompt: String,

    /// Override the system prompt.
    #[arg(short, long)]
    pub system: Option<String>,

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
    let registry = build_registry_with_breaker(&cfg, args.dispatch.breaker)?;
    let dispatch = build_dispatch(&cfg, &registry, &args.dispatch)?;

    // Resolve the prompt once: `-` means read stdin. Reused below for
    // both the request body and the trajectory `input_summary` length.
    let prompt = resolve_prompt(&args)?;
    let req = build_request(&args, &prompt);

    let cache_registry = build_cache(args.dispatch.cache_path.as_deref())?;
    let cache_factory = CacheKeyFactory::new(1);

    let pipeline = LlmService::builder_with_inner(dispatch.inner.clone())
        .layer(TelemetryMiddleware::new())
        .layer(CacheLookupMiddleware::new(
            cache_registry,
            cache_factory,
            dispatch.cache_origin_id.clone(),
        ))
        .layer(RetryMiddleware::default())
        .build();

    let ctx = RequestContext::test_default(); // no IAM/audit yet (M6)
    if args.dispatch.no_cache {
        set_cache_policy(&ctx, &CachePolicy::off());
    }

    // Trajectory log — best-effort. Default ON; --no-trajectory or a
    // missing XDG data dir skips. Any error mid-write degrades to a
    // tracing::warn so the LLM call itself isn't blocked by a
    // local-state hiccup (same Doc 03 §4.3 "best-effort, never fatal"
    // discipline the cache uses).
    let trajectory_logger = build_trajectory_logger(&args, &dispatch, prompt.chars().count()).await;

    let dispatch_label = dispatch.label.clone();

    let stream_result = pipeline.call(req, ctx).await;
    let stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            // Open-time failure — log StepFailed + abandon, then bubble.
            if let Some(logger) = &trajectory_logger {
                logger.record_open_failure(&e).await;
            }
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("opening stream via {dispatch_label}"));
        }
    };

    let outcome = drain_stream_to_stdout(stream).await;

    // Whatever happened (success / mid-stream error), close out the
    // trajectory before propagating.
    if let Some(logger) = &trajectory_logger {
        logger.record_outcome(&outcome, &dispatch).await;
    }

    let outcome = outcome?;
    if !args.no_summary {
        print_summary(
            dispatch.cost_provider.as_ref(),
            &dispatch.model_label,
            &outcome,
        );
        if let Some(logger) = &trajectory_logger {
            eprintln!("── trajectory: {}", logger.id());
        }
    }
    Ok(())
}

/// Holds the per-invocation trajectory + the runtime handle that
/// writes to it. Lifecycle: `start_for_run()` writes
/// `TrajectoryStarted + StepStarted`; later `record_outcome()`
/// writes the rest.
///
/// All methods swallow errors with a `tracing::warn` rather than
/// propagating — trajectory logging is observability, not the
/// critical path. A SQLite hiccup must not block the user's LLM
/// response.
struct TrajectoryLogger {
    runtime: Arc<LocalRuntime>,
    traj: TrajectoryId,
    // Note: the StepStarted event already carries its idempotency key
    // — recovery code that wants it reads `runtime.replay(&traj)` and
    // pulls it from there. We don't keep an extra copy on the logger.
}

impl TrajectoryLogger {
    fn id(&self) -> &TrajectoryId {
        &self.traj
    }

    async fn record_open_failure(&self, err: &tars_types::ProviderError) {
        let class = format!("{:?}", err.class()).to_lowercase();
        if let Err(e) = self
            .runtime
            .append(
                &self.traj,
                AgentEvent::StepFailed {
                    traj: self.traj.clone(),
                    step_seq: 1,
                    error: format!("{err}"),
                    classification: class,
                },
            )
            .await
        {
            tracing::warn!(error = %e, "trajectory: failed to record StepFailed");
        }
        if let Err(e) = self
            .runtime
            .append(
                &self.traj,
                AgentEvent::TrajectoryAbandoned {
                    traj: self.traj.clone(),
                    cause: format!("open-time error: {err}"),
                },
            )
            .await
        {
            tracing::warn!(error = %e, "trajectory: failed to record TrajectoryAbandoned");
        }
    }

    async fn record_outcome(&self, outcome: &Result<StreamOutcome>, dispatch: &Dispatch) {
        match outcome {
            Ok(o) => {
                if let Err(e) = self
                    .runtime
                    .append(
                        &self.traj,
                        AgentEvent::LlmCallCaptured {
                            traj: self.traj.clone(),
                            step_seq: 1,
                            provider: dispatch.cache_origin_id.clone(),
                            prompt_summary: format!(
                                "see step's input_summary; model={}",
                                dispatch.model_label
                            ),
                            response_summary: response_summary(o),
                            usage: o.usage,
                            // tars run is a single-call ad-hoc path
                            // (no agent loop); the audit-critical
                            // multi-step trajectories from tars
                            // run-task DO populate this hash via
                            // execute_agent_step. Threading the
                            // system prompt down to here is a
                            // separate small refactor — None for now.
                            system_prompt_hash: None,
                            // Single-call path: no agent tool-loop, so no
                            // cross-call tool trajectory to record (Doc 26 M2).
                            tool_calls: Vec::new(),
                            tool_call_args: Vec::new(),
                        },
                    )
                    .await
                {
                    tracing::warn!(error = %e, "trajectory: failed to record LlmCallCaptured");
                }
                if let Err(e) = self
                    .runtime
                    .append(
                        &self.traj,
                        AgentEvent::StepCompleted {
                            traj: self.traj.clone(),
                            step_seq: 1,
                            output_summary: response_summary(o),
                            usage: o.usage,
                        },
                    )
                    .await
                {
                    tracing::warn!(error = %e, "trajectory: failed to record StepCompleted");
                }
                let stop = o
                    .stop_reason
                    .map(|s| format!("{s:?}"))
                    .unwrap_or_else(|| "no-finished".into());
                if let Err(e) = self
                    .runtime
                    .append(
                        &self.traj,
                        AgentEvent::TrajectoryCompleted {
                            traj: self.traj.clone(),
                            summary: format!(
                                "stop={stop}; tokens in={} out={}",
                                o.usage.input_tokens, o.usage.output_tokens
                            ),
                        },
                    )
                    .await
                {
                    tracing::warn!(error = %e, "trajectory: failed to record TrajectoryCompleted");
                }
            }
            Err(e) => {
                if let Err(append_err) = self
                    .runtime
                    .append(
                        &self.traj,
                        AgentEvent::StepFailed {
                            traj: self.traj.clone(),
                            step_seq: 1,
                            error: format!("{e:#}"),
                            classification: "stream_error".into(),
                        },
                    )
                    .await
                {
                    tracing::warn!(error = %append_err, "trajectory: failed to record StepFailed");
                }
                if let Err(append_err) = self
                    .runtime
                    .append(
                        &self.traj,
                        AgentEvent::TrajectoryAbandoned {
                            traj: self.traj.clone(),
                            cause: format!("mid-stream error: {e:#}"),
                        },
                    )
                    .await
                {
                    tracing::warn!(error = %append_err, "trajectory: failed to record TrajectoryAbandoned");
                }
            }
        }
    }
}

fn response_summary(o: &StreamOutcome) -> String {
    // Doc 04 says payloads are "small (<4KB), large goes to ContentStore".
    // For now we keep a 200-char head to stay well under the cap;
    // ContentStore (D-1 / future B-7 follow-up) replaces this.
    // Single pass: take 201 chars, then check whether a 201st existed
    // rather than re-walking the whole string with a second
    // `chars().count()` (O(n) twice on a potentially large response).
    let mut chars = o.response_text.chars();
    let head: String = chars.by_ref().take(200).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

async fn build_trajectory_logger(
    args: &RunArgs,
    dispatch: &Dispatch,
    prompt_chars: usize,
) -> Option<TrajectoryLogger> {
    if args.dispatch.no_trajectory {
        return None;
    }
    let store = match event_store::open(args.dispatch.events_path.as_deref()) {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!("trajectory: no XDG data dir on this platform; skipping log",);
            return None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "trajectory: opening event store failed; skipping log");
            return None;
        }
    };
    let runtime = LocalRuntime::new(store);

    // Reason carries the dispatch label so `tars trajectory show`
    // surfaces what was wired without re-parsing the events.
    let reason = format!("tars run via {}", dispatch.label);
    let traj = match runtime.create_trajectory(None, &reason).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "trajectory: create_trajectory failed; skipping log");
            return None;
        }
    };

    let input_summary = format!(
        // `prompt_chars` is the resolved prompt's char count (threaded
        // from execute, which also handles the `-` => stdin case); the
        // length signal is enough for log-grep + future replay.
        // build_request stamped req.model = Explicit(model_label),
        // so model_label is the right thing to record.
        "prompt({prompt_chars} chars), model={}",
        dispatch.model_label,
    );
    // Note on partial writes: an ungraceful shutdown (SIGKILL, panic)
    // between this StepStarted and the terminal event from
    // record_outcome/record_open_failure leaves the trajectory in the
    // `active` state with no terminal event. This is intentional and
    // recoverable: the append log is the source of truth, `tars
    // trajectory list` renders such a trajectory as `active`, and the
    // StepStarted carries an idempotency key so a future
    // resume/replay can pick up exactly where it left off. We do NOT
    // try to fsync-fence each event — best-effort observability per the
    // TrajectoryLogger doc above and Doc 03 §4.3.
    let key = StepIdempotencyKey::compute(&traj, 1, &input_summary);
    let agent = if dispatch.label.starts_with("tier") {
        "tars-cli/run/tier".to_string()
    } else {
        "tars-cli/run/single-provider".to_string()
    };
    if let Err(e) = runtime
        .append(
            &traj,
            AgentEvent::StepStarted {
                traj: traj.clone(),
                step_seq: 1,
                agent,
                idempotency_key: key.clone(),
                input_summary,
            },
        )
        .await
    {
        tracing::warn!(error = %e, "trajectory: StepStarted append failed; skipping further logging");
        return None;
    }

    Some(TrajectoryLogger { runtime, traj })
}

/// Resolve the prompt source. The documented `-` sentinel reads the
/// whole prompt from stdin (so callers can pipe large prompts without
/// blowing the arg length limit); any other value is used verbatim.
fn resolve_prompt(args: &RunArgs) -> Result<String> {
    if args.prompt == "-" {
        use std::io::Read as _;
        // Cap the stdin read so a `tars run --prompt - < /dev/zero` (or
        // an accidental binary pipe) can't OOM the process. 8 MiB is far
        // larger than any realistic prompt yet bounds worst case.
        const STDIN_PROMPT_CAP: u64 = 8 * 1024 * 1024;
        let mut buf = String::new();
        let read = std::io::stdin()
            .take(STDIN_PROMPT_CAP)
            .read_to_string(&mut buf)
            .context("reading prompt from stdin (`--prompt -`)")?;
        if read as u64 == STDIN_PROMPT_CAP {
            anyhow::bail!(
                "stdin prompt exceeds {} byte cap (`--prompt -`); pass a smaller prompt",
                STDIN_PROMPT_CAP
            );
        }
        Ok(buf)
    } else {
        Ok(args.prompt.clone())
    }
}

fn build_request(args: &RunArgs, prompt: &str) -> ChatRequest {
    let mut req = ChatRequest::user(prompt);
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
    /// The model that actually served this response, taken from the
    /// `Started` event. This is the honest key for per-model pricing:
    /// a provider may route `--model foo` to a concrete `foo-2025-xx`,
    /// and cost must be attributed to what actually answered.
    pub actual_model: String,
    pub usage: Usage,
    pub stop_reason: Option<tars_types::StopReason>,
    /// True if we ever wrote *something* to stdout. Lets us print
    /// a leading newline before the summary only when needed.
    pub wrote_anything: bool,
    /// Cache info from the Started event. Non-zero
    /// `cached_input_tokens` means we replayed a hit.
    pub cache_hit: CacheHitInfo,
    /// Concatenated text deltas. Captured in addition to streaming
    /// to stdout so the trajectory log can record a head-of-response
    /// summary without re-reading the network. Bounded by whatever
    /// the model emitted — for very long responses we summarise on
    /// the way into the event log.
    pub response_text: String,
}

/// Soft cap on the in-memory copy of the response kept for the
/// trajectory summary. The summary itself is only ~200 chars; 8 KiB
/// is generous headroom while bounding worst-case memory for a runaway
/// stream. The full response always reaches stdout regardless.
const RESPONSE_TEXT_CAP: usize = 8 * 1024;

async fn drain_stream_to_stdout(
    mut stream: tars_provider::LlmEventStream,
) -> Result<StreamOutcome> {
    let mut outcome = StreamOutcome::default();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    while let Some(ev) = stream.next().await {
        match ev.context("stream error")? {
            ChatEvent::Started {
                cache_hit,
                actual_model,
            } => {
                outcome.cache_hit = cache_hit;
                outcome.actual_model = actual_model;
            }
            ChatEvent::Delta { text } => {
                out.write_all(text.as_bytes()).context("stdout write")?;
                out.flush().context("stdout flush")?;
                outcome.wrote_anything = !text.is_empty() || outcome.wrote_anything;
                // Full text always streams to stdout above; `response_text`
                // is only the trajectory summary source (head-of-response).
                // Cap accumulation so a huge/runaway stream can't OOM the
                // CLI just to feed a 200-char summary.
                if outcome.response_text.len() < RESPONSE_TEXT_CAP {
                    outcome.response_text.push_str(&text);
                }
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

fn print_summary(
    provider: &dyn tars_provider::LlmProvider,
    requested_model: &str,
    outcome: &StreamOutcome,
) {
    let cost = cost_for_outcome(provider, requested_model, outcome);
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

/// USD cost for a finished stream, priced by the model that ACTUALLY
/// served it (`outcome.actual_model`) rather than a per-provider rate.
///
/// Every backend builds its `Capabilities` with `Pricing::default()`
/// (all zeros) — a deliberate choice, since a provider serves many
/// models at different rates, so there is no single honest per-provider
/// price. The real per-model rates live in the model KB
/// (`data/models.toml`), keyed by model id. Resolution order:
///
/// 1. `actual_model` — the id the provider reported serving. Most
///    precise: a routing/tier layer may serve a different model than
///    asked for, and cost must follow what answered.
/// 2. `requested_model` — the model we asked for (`--model` / the
///    provider's `default_model`). This is the fallback for providers
///    that echo a dated snapshot id: OpenAI answers `gpt-5.4` as
///    `gpt-5.4-2026-03-05`, which is not a KB id, but the requested
///    `gpt-5.4` is — and it is the same model family / price. Keyed off
///    the requested label rather than string-munging the date, so it
///    stays correct as snapshots roll.
/// 3. `provider.cost()` — the provider's own per-provider rate. Correct
///    `$0` for subscription/local backends (local servers, CLI backends,
///    custom deployments — none of which are in the KB).
///
/// Never panics.
fn cost_for_outcome(
    provider: &dyn tars_provider::LlmProvider,
    requested_model: &str,
    outcome: &StreamOutcome,
) -> CostUsd {
    tars_config::MODEL_KB
        .pricing(&outcome.actual_model)
        .or_else(|| tars_config::MODEL_KB.pricing(requested_model))
        .map(|p| p.cost_for(&outcome.usage))
        .unwrap_or_else(|| provider.cost(&outcome.usage))
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
    // pick_provider / parse_tier / build_dispatch tests live in
    // dispatch.rs now that the helpers do — see its `tests` module.
    // This module keeps only run.rs-local tests (build_request,
    // format_cost, etc.).

    fn dispatch_args_default() -> DispatchArgs {
        DispatchArgs {
            provider: None,
            model: None,
            no_cache: false,
            cache_path: None,
            breaker: false,
            no_trajectory: false,
            events_path: None,
        }
    }

    #[test]
    fn build_request_propagates_overrides() {
        let args = RunArgs {
            dispatch: dispatch_args_default(),
            prompt: "hi".into(),
            system: Some("be brief".into()),
            max_output_tokens: Some(64),
            temperature: Some(0.3),
            no_summary: false,
        };
        let req = build_request(&args, "hi");
        assert_eq!(req.max_output_tokens, Some(64));
        assert_eq!(req.temperature, Some(0.3));
        assert_eq!(req.system.as_deref(), Some("be brief"));
        // Model is bound on the pipeline/service now, not the request.
    }

    #[test]
    fn format_cost_chooses_precision_by_magnitude() {
        assert_eq!(format_cost(CostUsd(0.0)), "$0 (free)");
        assert_eq!(format_cost(CostUsd(0.000_012)), "$0.000012");
        assert_eq!(format_cost(CostUsd(0.0123)), "$0.0123");
    }

    fn outcome_for(model: &str, input: u64, output: u64) -> StreamOutcome {
        StreamOutcome {
            actual_model: model.into(),
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn cost_uses_kb_pricing_for_a_known_actual_model() {
        // A provider whose own `Pricing` is the all-zeros default (the
        // reality for openai/anthropic/deepseek backends) must STILL
        // report real cost, because pricing is resolved per-model from
        // the KB via `actual_model`. `gpt-5.4` is a KB model at
        // $2.50/1M input, so 1M input tokens = $2.50.
        use tars_provider::LlmProvider as _;
        let provider =
            tars_provider::MockProvider::new("openai", tars_provider::CannedResponse::text("hi"));
        // Sanity: the provider's own per-provider price really is $0.
        assert_eq!(
            provider.cost(&outcome_for("gpt-5.4", 1_000_000, 0).usage),
            CostUsd(0.0)
        );

        let cost =
            cost_for_outcome(provider.as_ref(), "gpt-5.4", &outcome_for("gpt-5.4", 1_000_000, 0));
        assert!(
            (cost.as_f64() - 2.50).abs() < 1e-9,
            "expected KB-priced $2.50 for 1M gpt-5.4 input tokens, got {}",
            cost.as_f64()
        );
    }

    #[test]
    fn cost_falls_back_to_requested_model_for_a_dated_snapshot() {
        // OpenAI answers `gpt-5.4` with a dated snapshot id like
        // `gpt-5.4-2026-03-05` that is NOT itself a KB id. Pricing must
        // still resolve via the requested `gpt-5.4` label (same family
        // / price) rather than collapsing to $0.
        let provider =
            tars_provider::MockProvider::new("openai", tars_provider::CannedResponse::text("hi"));
        let cost = cost_for_outcome(
            provider.as_ref(),
            "gpt-5.4",
            &outcome_for("gpt-5.4-2026-03-05", 1_000_000, 0),
        );
        assert!(
            (cost.as_f64() - 2.50).abs() < 1e-9,
            "dated snapshot should price via requested gpt-5.4 ($2.50), got {}",
            cost.as_f64()
        );
    }

    #[test]
    fn cost_falls_back_to_provider_pricing_for_unknown_model_without_panicking() {
        // A model absent from the KB (local server / CLI backend /
        // custom deployment) — neither actual nor requested id is in the
        // KB — must not panic; it falls back to the provider's own
        // `cost()`, the correct $0 for a subscription/zero-priced backend.
        let provider =
            tars_provider::MockProvider::new("local", tars_provider::CannedResponse::text("hi"));
        let cost = cost_for_outcome(
            provider.as_ref(),
            "some-unlisted-local-model-xyz",
            &outcome_for("some-unlisted-local-model-xyz", 1_000_000, 1_000_000),
        );
        assert_eq!(cost, CostUsd(0.0));
    }
}
