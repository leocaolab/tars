//! `tars bench <provider>` — measure inference speed.
//!
//! Runs the same prompt against a configured provider N times and
//! reports TTFB (stream open → first text Delta) and decode rate
//! (output tokens / decode time, excluding TTFB) as mean / p50 / p99.
//!
//! ## Why this exists
//!
//! Local model comparison is a real workflow: a user with LM Studio
//! / mlx_lm.server / llama-server wants to pick between Qwen2.5-7B vs
//! Qwen2.5-32B vs Llama-3-8B based on actual measured throughput on
//! their hardware, not generic benchmark numbers. Cloud providers
//! also benefit from this when the user wants to confirm "is this
//! API still hitting the latency I assumed?".
//!
//! ## Usage
//!
//! ```bash
//! tars bench lmstudio
//! tars bench lmstudio --repeat 10 --warmup 2
//! tars bench lmstudio --model qwen/qwen2.5-coder-32b-instruct
//! tars bench lmstudio --prompt "Write a Rust function that reverses a linked list."
//! ```
//!
//! Output goes to stdout as a clean per-iter table plus a summary
//! block (so it's easy to paste / pipe / diff between runs). Status
//! / iteration progress goes to stderr.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use futures::StreamExt;

use tars_provider::auth::basic;
use tars_provider::http_base::HttpProviderBase;
use tars_provider::registry::ProviderRegistry;
use tars_types::{ChatEvent, ChatRequest, ModelHint, ProviderId, RequestContext};

use crate::config_loader;

/// Default benchmark prompt. Chosen to elicit a coherent response
/// of ~80-200 output tokens — short enough to keep iterations
/// reasonable, long enough that decode-rate measurements aren't
/// dominated by TTFB noise.
const DEFAULT_PROMPT: &str = "Write a small Rust function that returns the nth Fibonacci number using \
iteration. Include a doc comment explaining the parameter and return value. Do NOT include `fn main()` \
or example code — just the function.";

/// Hard upper bound on a single iteration's stream consumption. Bench
/// is a measurement tool; if a provider hangs (network partition,
/// server deadlock, infinite stream), we want a clear error rather
/// than a process that blocks forever.
const ITER_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Provider id from your config. Any type works (CLI / HTTP /
    /// local OpenAI-compat) but the most useful target is local
    /// inference servers (`lmstudio` / `mlx` / `llamacpp`).
    pub provider: String,

    /// Override the provider's `default_model`.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Override the prompt. Pick something that elicits
    /// ~80-200 tokens of output for sensible decode-rate readings.
    #[arg(short, long)]
    pub prompt: Option<String>,

    /// Number of measured iterations. Default 5 — enough to compute
    /// p50 / p99 without burning huge cost on cloud providers.
    #[arg(short, long, default_value_t = 5)]
    pub repeat: u32,

    /// Number of warmup iterations whose timings are discarded.
    /// Default 2 — local model servers (LM Studio / llama.cpp / mlx)
    /// often need a couple of cold-load + first-token-of-session
    /// passes before steady state. Cloud APIs only really need 1.
    /// Iters with `out=0` (model still loading; see warning in
    /// summary) are also excluded from stats automatically.
    #[arg(short, long, default_value_t = 2)]
    pub warmup: u32,

    /// Cap output tokens per iteration. Capping makes cross-model
    /// comparison fair (different models pick different output
    /// lengths for the same prompt; capping forces "how fast does
    /// each model generate the same N tokens?") AND keeps each iter
    /// fast for local-model bench loops. `None` lets the model
    /// decide.
    #[arg(long, default_value_t = 100)]
    pub max_tokens: u32,
}

#[derive(Debug)]
struct Sample {
    ttfb: Duration,
    total: Duration,
    in_tokens: u64,
    out_tokens: u64,
    thinking_tokens: u64,
}

impl Sample {
    /// Total tokens generated across both channels (visible + reasoning).
    /// Used as the numerator for decode rate so reasoning models like
    /// Qwen3-thinking / DeepSeek-R1 / o1 don't appear "0 tok/s" just
    /// because they put output in `reasoning_content` rather than
    /// `content`.
    fn generated_tokens(&self) -> u64 {
        self.out_tokens.saturating_add(self.thinking_tokens)
    }

    /// Decode tokens-per-second: total generated tokens (visible +
    /// reasoning) divided by the time AFTER the first generated
    /// token arrives. Excludes connection / queue / first-token
    /// latency so this is the model's pure generation speed. Returns
    /// 0.0 when no tokens were generated (avoid div-by-zero spam).
    fn decode_tok_per_sec(&self) -> f64 {
        let decode_time = self.total.saturating_sub(self.ttfb);
        let secs = decode_time.as_secs_f64();
        let total_gen = self.generated_tokens();
        if total_gen == 0 || secs <= 0.0 {
            return 0.0;
        }
        total_gen as f64 / secs
    }
}

pub async fn execute(args: BenchArgs, config_path: Option<PathBuf>) -> Result<()> {
    let cfg = config_loader::load(config_path)?;
    let provider_id = ProviderId::new(args.provider.clone());
    let provider_cfg = cfg.providers.get(&provider_id).ok_or_else(|| {
        let configured: Vec<String> = cfg.providers.iter().map(|(id, _)| id.to_string()).collect();
        anyhow::anyhow!(
            "provider `{}` not in config. Configured: [{}]",
            args.provider,
            configured.join(", "),
        )
    })?;

    let model = args
        .model
        .clone()
        .unwrap_or_else(|| provider_cfg.default_model().to_string());
    let prompt = args.prompt.as_deref().unwrap_or(DEFAULT_PROMPT);

    let http = HttpProviderBase::default_arc().context("constructing reqwest client")?;
    let registry = ProviderRegistry::from_config(&cfg.providers, http, basic())
        .context("building provider registry from config")?;
    let provider = registry
        .get(&provider_id)
        .ok_or_else(|| anyhow::anyhow!("registry missing provider `{provider_id}`"))?;

    eprintln!(
        "── tars bench {} (model={model}, prompt={} chars, max_tokens={}, {} warmup + {} measured) ──",
        args.provider,
        prompt.chars().count(),
        args.max_tokens,
        args.warmup,
        args.repeat,
    );

    // Warmup loops — same call shape, results discarded. Useful for
    // cold-cache / connection-establishment effects + LM-Studio-style
    // first-token-of-session warmup.
    for i in 0..args.warmup {
        eprint!("  warmup {}/{}  ", i + 1, args.warmup);
        match run_one(provider.clone(), &model, prompt, args.max_tokens).await {
            Ok(s) => eprintln!(
                "ttfb={:>6.0}ms  total={:>6.2}s  out={}  decode={:>5.1} tok/s",
                s.ttfb.as_secs_f64() * 1000.0,
                s.total.as_secs_f64(),
                s.out_tokens,
                s.decode_tok_per_sec(),
            ),
            Err(e) => {
                eprintln!("FAILED");
                return Err(e).with_context(|| {
                    format!(
                        "warmup iteration {}/{} failed; aborting before measured runs",
                        i + 1,
                        args.warmup,
                    )
                });
            }
        }
    }

    let mut samples: Vec<Sample> = Vec::with_capacity(args.repeat as usize);
    for i in 0..args.repeat {
        eprint!("  iter   {}/{}  ", i + 1, args.repeat);
        let s = run_one(provider.clone(), &model, prompt, args.max_tokens)
            .await
            .with_context(|| format!("iteration {} failed", i + 1))?;
        eprintln!(
            "ttfb={:>6.0}ms  total={:>6.2}s  out={}  decode={:>5.1} tok/s",
            s.ttfb.as_secs_f64() * 1000.0,
            s.total.as_secs_f64(),
            s.out_tokens,
            s.decode_tok_per_sec(),
        );
        samples.push(s);
    }

    print_summary(&args.provider, &model, &samples);
    Ok(())
}

async fn run_one(
    provider: Arc<dyn tars_provider::LlmProvider>,
    model: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<Sample> {
    let mut req = ChatRequest::user(ModelHint::Explicit(model.into()), prompt);
    req.max_output_tokens = Some(max_tokens);
    let started_at = Instant::now();
    let mut stream = Arc::clone(&provider)
        .stream(req, RequestContext::test_default())
        .await
        .context("stream() failed")?;

    let mut ttfb: Option<Duration> = None;
    let mut last_usage = tars_types::Usage::default();
    let consume = async {
        while let Some(ev) = stream.next().await {
            let ev = ev.context("stream item errored")?;
            match ev {
                // TTFB = first generated token (visible OR reasoning).
                // Reasoning-only output streams (o1 / DeepSeek-R1 in
                // some configs) would otherwise never fire a Delta and
                // TTFB would falsely fall back to total.
                ChatEvent::Delta { .. } | ChatEvent::ThinkingDelta { .. } if ttfb.is_none() => {
                    ttfb = Some(started_at.elapsed());
                }
                ChatEvent::Finished { usage, .. } => {
                    last_usage = usage;
                }
                // ThinkingDelta / Started / tool events: don't count as TTFB
                // (TTFB is "first user-visible token") and don't terminate.
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(ITER_TIMEOUT, consume)
        .await
        .with_context(|| {
            format!(
                "stream did not complete within {}s — provider hung or stalled",
                ITER_TIMEOUT.as_secs(),
            )
        })??;
    let total = started_at.elapsed();
    Ok(Sample {
        // Fall back to total if no Delta arrived (possible for empty
        // outputs); decode_tok_per_sec will then be 0.0.
        ttfb: ttfb.unwrap_or(total),
        total,
        in_tokens: last_usage.input_tokens,
        out_tokens: last_usage.output_tokens,
        thinking_tokens: last_usage.thinking_tokens,
    })
}

fn print_summary(provider_id: &str, model: &str, samples: &[Sample]) {
    if samples.is_empty() {
        return;
    }

    // Anomalous iters: zero generated tokens despite having spent
    // real wall time. Almost always means LM Studio (or another
    // local server) was still loading the model on this iter and
    // returned an empty response. Including them tanks the mean.
    let anomalous = samples.iter().filter(|s| s.generated_tokens() == 0).count();
    let valid: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.generated_tokens() > 0)
        .collect();

    if valid.is_empty() {
        println!();
        println!("── stats ── (provider={provider_id}, model={model})");
        println!(
            "  ALL {} iterations had 0 generated tokens — model probably never loaded.",
            samples.len(),
        );
        println!("  Try bumping --warmup or check the server logs.");
        return;
    }

    let n = valid.len();
    let mut ttfbs_ms: Vec<f64> = valid
        .iter()
        .map(|s| s.ttfb.as_secs_f64() * 1000.0)
        .collect();
    let mut totals_s: Vec<f64> = valid.iter().map(|s| s.total.as_secs_f64()).collect();
    let mut decodes: Vec<f64> = valid.iter().map(|s| s.decode_tok_per_sec()).collect();
    let mut outs: Vec<u64> = valid.iter().map(|s| s.out_tokens).collect();
    let mut ins: Vec<u64> = valid.iter().map(|s| s.in_tokens).collect();
    let thinking_total: u64 = valid.iter().map(|s| s.thinking_tokens).sum();

    // `partial_cmp` would panic on NaN. Current code paths can't
    // produce NaN (Duration::as_secs_f64 is finite; decode_tok_per_sec
    // guards div-by-zero), but a fallback to `Equal` keeps stats
    // generation robust if a future calculation slips one in.
    let cmp = |a: &f64, b: &f64| a.partial_cmp(b).unwrap_or(Ordering::Equal);
    ttfbs_ms.sort_by(cmp);
    totals_s.sort_by(cmp);
    decodes.sort_by(cmp);
    outs.sort_unstable();
    ins.sort_unstable();

    let mean_f = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let mean_u = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len() as f64;
    let p50_f = |v: &[f64]| v[v.len() / 2];
    let p99_f = |v: &[f64]| {
        // For n < 100 this isn't a true p99 — clamp to the worst sample.
        let idx = ((v.len() as f64 * 0.99).ceil() as usize)
            .saturating_sub(1)
            .min(v.len() - 1);
        v[idx]
    };

    println!();
    if anomalous > 0 {
        println!(
            "── stats ── (provider={provider_id}, model={model}, n={n} valid + {anomalous} skipped)",
        );
        println!(
            "  ⚠  {anomalous} iter(s) had 0 generated tokens (model still loading?) — \
             excluded from stats. Bump --warmup to absorb them.",
        );
    } else {
        println!("── stats ── (provider={provider_id}, model={model}, n={n})");
    }
    println!(
        "  TTFB     mean={:>7.0}ms   p50={:>7.0}ms   p99={:>7.0}ms",
        mean_f(&ttfbs_ms),
        p50_f(&ttfbs_ms),
        p99_f(&ttfbs_ms),
    );
    println!(
        "  Total    mean={:>7.2}s    p50={:>7.2}s    p99={:>7.2}s",
        mean_f(&totals_s),
        p50_f(&totals_s),
        p99_f(&totals_s),
    );
    println!(
        "  Decode   mean={:>7.1}     p50={:>7.1}     p99={:>7.1}     tok/s",
        mean_f(&decodes),
        p50_f(&decodes),
        p99_f(&decodes),
    );
    println!(
        "  Out      mean={:>7.1}                                  tokens",
        mean_u(&outs),
    );
    println!(
        "  In       mean={:>7.1}                                  tokens",
        mean_u(&ins),
    );
    if thinking_total > 0 {
        println!("  (Thinking tokens reported across all iterations: {thinking_total})",);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ttfb_ms: u64, total_ms: u64, out: u64) -> Sample {
        Sample {
            ttfb: Duration::from_millis(ttfb_ms),
            total: Duration::from_millis(total_ms),
            in_tokens: 12,
            out_tokens: out,
            thinking_tokens: 0,
        }
    }

    #[test]
    fn decode_rate_excludes_ttfb() {
        // 100 tokens in 1.0s of decode (1.5s total - 0.5s ttfb) → 100 tok/s.
        let sample = s(500, 1500, 100);
        assert!((sample.decode_tok_per_sec() - 100.0).abs() < 0.01);
    }

    #[test]
    fn decode_rate_zero_when_no_output() {
        let sample = s(100, 200, 0);
        assert_eq!(sample.decode_tok_per_sec(), 0.0);
    }

    #[test]
    fn decode_rate_zero_when_decode_time_zero() {
        // ttfb >= total → no decode window → return 0 instead of NaN/inf.
        let sample = s(500, 500, 100);
        assert_eq!(sample.decode_tok_per_sec(), 0.0);
    }
}
