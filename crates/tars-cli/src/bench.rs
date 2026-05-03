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
    /// Default 1 — covers the cold-cache / first-token-of-session
    /// latency that's not representative of steady-state throughput.
    #[arg(short, long, default_value_t = 1)]
    pub warmup: u32,
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
    /// Decode tokens-per-second: output tokens divided by the time
    /// AFTER the first byte. Excludes connection / queue / first-
    /// token latency so this is the model's pure generation speed.
    /// Returns 0.0 when output is empty (avoid div-by-zero spam).
    fn decode_tok_per_sec(&self) -> f64 {
        let decode_time = self.total.saturating_sub(self.ttfb);
        let secs = decode_time.as_secs_f64();
        if self.out_tokens == 0 || secs <= 0.0 {
            return 0.0;
        }
        self.out_tokens as f64 / secs
    }
}

pub async fn execute(args: BenchArgs, config_path: Option<PathBuf>) -> Result<()> {
    let cfg = config_loader::load(config_path)?;
    let provider_id = ProviderId::new(args.provider.clone());
    let provider_cfg = cfg.providers.get(&provider_id).ok_or_else(|| {
        let configured: Vec<String> =
            cfg.providers.iter().map(|(id, _)| id.to_string()).collect();
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
    let provider = registry.get(&provider_id).ok_or_else(|| {
        anyhow::anyhow!("registry missing provider `{provider_id}`")
    })?;

    eprintln!(
        "── tars bench {} (model={model}, prompt={} chars, {} warmup + {} measured) ──",
        args.provider,
        prompt.chars().count(),
        args.warmup,
        args.repeat,
    );

    // Warmup loops — same call shape, results discarded. Useful for
    // cold-cache / connection-establishment effects + LM-Studio-style
    // first-token-of-session warmup.
    for i in 0..args.warmup {
        eprint!("  warmup {}/{}  ", i + 1, args.warmup);
        match run_one(provider.clone(), &model, prompt).await {
            Ok(s) => eprintln!(
                "ttfb={:>6.0}ms  total={:>6.2}s  out={}  decode={:>5.1} tok/s",
                s.ttfb.as_secs_f64() * 1000.0,
                s.total.as_secs_f64(),
                s.out_tokens,
                s.decode_tok_per_sec(),
            ),
            Err(e) => {
                eprintln!("FAILED: {e:?}");
                anyhow::bail!("warmup iteration failed; aborting before measured runs");
            }
        }
    }

    let mut samples: Vec<Sample> = Vec::with_capacity(args.repeat as usize);
    for i in 0..args.repeat {
        eprint!("  iter   {}/{}  ", i + 1, args.repeat);
        let s = run_one(provider.clone(), &model, prompt)
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
) -> Result<Sample> {
    let req = ChatRequest::user(ModelHint::Explicit(model.into()), prompt);
    let started_at = Instant::now();
    let mut stream = Arc::clone(&provider)
        .stream(req, RequestContext::test_default())
        .await
        .context("stream() failed")?;

    let mut ttfb: Option<Duration> = None;
    let mut last_usage = tars_types::Usage::default();
    while let Some(ev) = stream.next().await {
        let ev = ev.context("stream item errored")?;
        match ev {
            ChatEvent::Delta { .. } if ttfb.is_none() => {
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

    let n = samples.len();
    let mut ttfbs_ms: Vec<f64> =
        samples.iter().map(|s| s.ttfb.as_secs_f64() * 1000.0).collect();
    let mut totals_s: Vec<f64> = samples.iter().map(|s| s.total.as_secs_f64()).collect();
    let mut decodes: Vec<f64> = samples.iter().map(Sample::decode_tok_per_sec).collect();
    let mut outs: Vec<u64> = samples.iter().map(|s| s.out_tokens).collect();
    let mut ins: Vec<u64> = samples.iter().map(|s| s.in_tokens).collect();
    let thinking_total: u64 = samples.iter().map(|s| s.thinking_tokens).sum();

    ttfbs_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    totals_s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    decodes.sort_by(|a, b| a.partial_cmp(b).unwrap());
    outs.sort_unstable();
    ins.sort_unstable();

    let mean_f = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let mean_u = |v: &[u64]| v.iter().sum::<u64>() as f64 / v.len() as f64;
    let p50_f = |v: &[f64]| v[v.len() / 2];
    let p99_f = |v: &[f64]| {
        // For n < 100 this isn't a true p99 — clamp to the worst sample.
        let idx = ((v.len() as f64 * 0.99).ceil() as usize).saturating_sub(1).min(v.len() - 1);
        v[idx]
    };

    println!();
    println!("── stats ── (provider={provider_id}, model={model}, n={n})");
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
        println!(
            "  (Thinking tokens reported across all iterations: {thinking_total})",
        );
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
