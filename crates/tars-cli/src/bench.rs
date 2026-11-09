//! Runs `tars bench <provider>`: sends the same prompt N times and reports TTFB (request sent to
//! first generated token, visible or reasoning) and decode rate (generated tokens over the time
//! after TTFB) as mean / p50 / p99, so local models can be compared on the user's own hardware.
//!
//! The summary goes to stdout, so it can be piped or diffed between runs; each iteration's line
//! goes to stderr.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Args;
use futures::StreamExt;

use tars_llm::provider::registry::RegistryError;
use tars_llm::{LlmError, LlmService, ProviderError};
use tars_tools::SandboxMode;
use tars_types::{Caller, ChatRequest, ProviderId, TenantId};

use crate::Ending;
use crate::config::{self, LoadConfigError};
use crate::home_env::HomeEnvError;
use crate::registry::{NotConfiguredError, build_registry, configured};
use crate::single_attempt;
use crate::transcript::Transcript;

/// Asks for more output than the default `--max-tokens` of 100, so every iteration decodes to the
/// cap and the decode rate is not TTFB noise.
const DEFAULT_PROMPT: &str = "Write a small Rust function that returns the nth Fibonacci number using \
iteration. Include a doc comment explaining the parameter and return value. Do NOT include `fn main()` \
or example code — just the function.";

/// Bounds one iteration, opening the stream included, so a provider that hangs is an error rather
/// than a bench that never ends.
const ITER_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Provider id from your config. Any type works; local inference
    /// servers (`lmstudio` / `mlx` / `llamacpp`) are the usual target.
    #[arg(value_parser = ProviderId::from_str)]
    pub provider: ProviderId,

    /// The tenant the benchmark calls are made for.
    #[arg(long, value_parser = TenantId::from_str)]
    pub tenant: TenantId,

    /// Override the provider's `default_model`; a `<series>@latest`
    /// spec resolves against the provider's catalog, as in the config.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Override the prompt. Pick one that asks for more output than
    /// `--max-tokens`, so every iteration decodes to the cap.
    #[arg(short, long)]
    pub prompt: Option<String>,

    /// Number of measured iterations; below 100 the reported p99 is the
    /// slowest one. Iterations that generate no token at all (visible or
    /// reasoning) are left out of the stats, with a warning.
    #[arg(short, long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
    pub repeat: u32,

    /// Number of warmup iterations whose timings are discarded. Local
    /// model servers (LM Studio / llama.cpp / mlx) often need a couple of
    /// cold-load + first-token-of-session passes before steady state;
    /// cloud APIs only really need 1.
    #[arg(short, long, default_value_t = 2)]
    pub warmup: u32,

    /// Cap output tokens per iteration; with a prompt that asks for more,
    /// every model is timed over the same number of tokens.
    #[arg(long, default_value_t = 100)]
    pub max_tokens: u32,
}

#[derive(Debug)]
struct Sample {
    /// When the first generated token arrived; `None` when none did.
    first_token: Option<Duration>,
    total: Duration,
    in_tokens: u64,
    out_tokens: u64,
    thinking_tokens: u64,
}

impl Sample {
    /// Returns visible plus reasoning tokens, so a reasoning model that answers in
    /// `reasoning_content` does not read as 0 tok/s.
    fn generated_tokens(&self) -> u64 {
        self.out_tokens.saturating_add(self.thinking_tokens)
    }

    /// Returns generated tokens per second of the time after the first token, or 0.0 when nothing
    /// was generated or no time passed.
    fn decode_tok_per_sec(&self) -> f64 {
        let Some(first_token) = self.first_token else {
            return 0.0;
        };
        let decode_time = self.total.saturating_sub(first_token);
        let secs = decode_time.as_secs_f64();
        let total_gen = self.generated_tokens();
        if total_gen == 0 || secs <= 0.0 {
            return 0.0;
        }
        total_gen as f64 / secs
    }
}

/// Why one iteration measured nothing.
#[derive(Debug, thiserror::Error)]
pub enum IterationError {
    #[error("the call was refused before its stream opened")]
    Refused(#[source] LlmError),
    #[error("stream item errored")]
    Stream(#[source] ProviderError),
    #[error("stream did not complete within {}s — provider hung or stalled", ITER_TIMEOUT.as_secs())]
    TimedOut(#[source] tokio::time::error::Elapsed),
    #[error("stream ended without a Finished event ({events} events received)")]
    Unfinished { events: usize },
}

/// Why `tars bench` measured nothing it can report.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error(transparent)]
    Config(#[from] LoadConfigError),
    #[error(transparent)]
    Env(#[from] HomeEnvError),
    #[error(transparent)]
    NotConfigured(#[from] NotConfiguredError),
    #[error("building provider registry from config")]
    Registry(#[source] RegistryError),
    #[error("provider `{provider}`")]
    Unavailable {
        provider: ProviderId,
        #[source]
        source: RegistryError,
    },
    #[error("--model for provider `{provider}`")]
    ModelUnresolved {
        provider: ProviderId,
        #[source]
        source: RegistryError,
    },
    #[error("warmup iteration {iteration}/{of} failed; aborting before measured runs")]
    Warmup {
        iteration: u32,
        of: u32,
        #[source]
        source: IterationError,
    },
    #[error("iteration {iteration} failed")]
    Iteration {
        iteration: u32,
        #[source]
        source: IterationError,
    },
    #[error("all {iterations} iterations had 0 generated tokens")]
    NothingGenerated { iterations: usize },
}

impl BenchError {
    /// Says whether the bench measured: only a run whose every iteration generated nothing is a
    /// measurement that failed; a broken iteration stops the bench before it has one.
    pub(crate) fn ending(&self) -> Ending {
        match self {
            Self::NothingGenerated { .. } => Ending::Failed,
            Self::Config(_)
            | Self::Env(_)
            | Self::NotConfigured(_)
            | Self::Registry(_)
            | Self::Unavailable { .. }
            | Self::ModelUnresolved { .. }
            | Self::Warmup { .. }
            | Self::Iteration { .. } => Ending::NotRun,
        }
    }
}

pub async fn execute(
    args: BenchArgs,
    config_path: Option<PathBuf>,
    sandbox: Option<SandboxMode>,
) -> Result<(), BenchError> {
    let cfg = config::load(config_path)?;
    let prompt = args.prompt.as_deref().unwrap_or(DEFAULT_PROMPT);

    let id = configured(&cfg, args.provider)?;
    let registry =
        build_registry(&cfg, Arc::new(crate::home_env::load()?)).map_err(BenchError::Registry)?;
    let provider = registry
        .get(&id)
        .map_err(|source| BenchError::Unavailable {
            provider: id.clone(),
            source,
        })?;
    let model = registry
        .resolve_model(&id, args.model.as_deref())
        .map_err(|source| BenchError::ModelUnresolved {
            provider: id.clone(),
            source,
        })?;

    let caller = crate::request_context_for(args.tenant.clone(), &cfg, sandbox);
    let service = single_attempt::service(provider, &model);

    eprintln!(
        "── tars bench {id} (model={model}, prompt={} chars, max_tokens={}, {} warmup + {} measured) ──",
        prompt.chars().count(),
        args.max_tokens,
        args.warmup,
        args.repeat,
    );

    for i in 0..args.warmup {
        eprint!("  warmup {}/{}  ", i + 1, args.warmup);
        match run_one(&service, &caller, prompt, args.max_tokens).await {
            Ok(s) => eprintln!("{}", iteration_line(&s)),
            Err(source) => {
                eprintln!("FAILED");
                return Err(BenchError::Warmup {
                    iteration: i + 1,
                    of: args.warmup,
                    source,
                });
            }
        }
    }

    let mut samples: Vec<Sample> = Vec::new();
    for i in 0..args.repeat {
        eprint!("  iter   {}/{}  ", i + 1, args.repeat);
        let s = match run_one(&service, &caller, prompt, args.max_tokens).await {
            Ok(s) => s,
            Err(source) => {
                eprintln!("FAILED");
                return Err(BenchError::Iteration {
                    iteration: i + 1,
                    source,
                });
            }
        };
        eprintln!("{}", iteration_line(&s));
        samples.push(s);
    }

    let summary = summarize(&samples);
    print!("{}", render_summary(id.as_str(), &model, &summary));
    if let Summary::NothingGenerated { iterations } = summary {
        return Err(BenchError::NothingGenerated { iterations });
    }
    Ok(())
}

async fn run_one(
    service: &LlmService,
    caller: &Caller,
    prompt: &str,
    max_tokens: u32,
) -> Result<Sample, IterationError> {
    let mut req = ChatRequest::user(prompt);
    req.max_output_tokens = Some(max_tokens);
    let started_at = Instant::now();
    let consume = async {
        let mut stream = service
            .call(req, caller.for_call())
            .await
            .result
            .map_err(IterationError::Refused)?;
        let mut transcript = Transcript::default();
        while let Some(ev) = stream.next().await {
            let event = ev.map_err(IterationError::Stream)?;
            transcript = transcript.step(event, started_at.elapsed());
        }
        Ok::<_, IterationError>(transcript)
    };
    let transcript = tokio::time::timeout(ITER_TIMEOUT, consume)
        .await
        .map_err(IterationError::TimedOut)??;
    let total = started_at.elapsed();
    let Some((_, usage)) = transcript.finished else {
        return Err(IterationError::Unfinished {
            events: transcript.events,
        });
    };
    Ok(Sample {
        first_token: transcript.first_token,
        total,
        in_tokens: usage.input_tokens,
        out_tokens: usage.output_tokens,
        thinking_tokens: usage.thinking_tokens,
    })
}

/// Returns the line printed for one iteration.
fn iteration_line(s: &Sample) -> String {
    let ttfb = s.first_token.map_or_else(
        || "  none".to_string(),
        |t| format!("{:>6.0}", t.as_secs_f64() * 1000.0),
    );
    format!(
        "ttfb={ttfb}ms  total={:>6.2}s  out={}  decode={:>5.1} tok/s",
        s.total.as_secs_f64(),
        s.out_tokens,
        s.decode_tok_per_sec(),
    )
}

/// Mean, median and 99th percentile of a non-empty set of values. Below 100 values the p99 is the
/// largest one.
#[derive(Debug, PartialEq)]
struct Stats {
    mean: f64,
    p50: f64,
    p99: f64,
}

impl Stats {
    fn of(first: f64, rest: impl IntoIterator<Item = f64>) -> Self {
        let mut v: Vec<f64> = std::iter::once(first).chain(rest).collect();
        v.sort_by(f64::total_cmp);
        let n = v.len();
        let p99 = ((n as f64 * 0.99).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        Self {
            mean: v.iter().sum::<f64>() / n as f64,
            p50: v[n / 2],
            p99: v[p99],
        }
    }
}

/// What a bench's measured iterations add up to.
#[derive(Debug, PartialEq)]
enum Summary {
    /// No iteration generated a token, or timed its first one.
    NothingGenerated { iterations: usize },
    Measured {
        /// The iterations that count, and the ones left out for generating nothing.
        n: usize,
        skipped: usize,
        ttfb_ms: Stats,
        total_s: Stats,
        decode: Stats,
        out_mean: f64,
        in_mean: f64,
        thinking_total: u64,
    },
}

/// Summarizes the iterations that generated tokens. One that generated none, or no first token,
/// is almost always a local server still loading the model; counted, it would drag the mean
/// down.
fn summarize(samples: &[Sample]) -> Summary {
    let valid: Vec<(&Sample, Duration)> = samples
        .iter()
        .filter(|s| s.generated_tokens() > 0)
        .filter_map(|s| s.first_token.map(|t| (s, t)))
        .collect();
    let Some(((first, first_ttfb), rest)) = valid.split_first() else {
        return Summary::NothingGenerated {
            iterations: samples.len(),
        };
    };
    let n = valid.len();
    let mean_of =
        |f: fn(&Sample) -> u64| valid.iter().map(|(s, _)| f(s) as f64).sum::<f64>() / n as f64;
    Summary::Measured {
        n,
        skipped: samples.len() - n,
        ttfb_ms: Stats::of(
            first_ttfb.as_secs_f64() * 1000.0,
            rest.iter().map(|(_, t)| t.as_secs_f64() * 1000.0),
        ),
        total_s: Stats::of(
            first.total.as_secs_f64(),
            rest.iter().map(|(s, _)| s.total.as_secs_f64()),
        ),
        decode: Stats::of(
            first.decode_tok_per_sec(),
            rest.iter().map(|(s, _)| s.decode_tok_per_sec()),
        ),
        out_mean: mean_of(|s| s.out_tokens),
        in_mean: mean_of(|s| s.in_tokens),
        thinking_total: valid
            .iter()
            .fold(0u64, |sum, (s, _)| sum.saturating_add(s.thinking_tokens)),
    }
}

/// Returns the summary as printed on stdout.
fn render_summary(provider_id: &str, model: &str, summary: &Summary) -> String {
    let mut out = String::from("\n");
    let (n, skipped, ttfb_ms, total_s, decode, out_mean, in_mean, thinking_total) = match summary {
        Summary::NothingGenerated { iterations } => {
            out.push_str(&format!(
                "── stats ── (provider={provider_id}, model={model})\n"
            ));
            out.push_str(&format!(
                "  ALL {iterations} iterations had 0 generated tokens — model probably never loaded.\n"
            ));
            out.push_str("  Try bumping --warmup or check the server logs.\n");
            return out;
        }
        Summary::Measured {
            n,
            skipped,
            ttfb_ms,
            total_s,
            decode,
            out_mean,
            in_mean,
            thinking_total,
        } => (
            n,
            skipped,
            ttfb_ms,
            total_s,
            decode,
            out_mean,
            in_mean,
            thinking_total,
        ),
    };
    if *skipped > 0 {
        out.push_str(&format!(
            "── stats ── (provider={provider_id}, model={model}, n={n} valid + {skipped} skipped)\n"
        ));
        out.push_str(&format!(
            "  ⚠  {skipped} iter(s) had 0 generated tokens (model still loading?) — \
             excluded from stats. Bump --warmup to absorb them.\n"
        ));
    } else {
        out.push_str(&format!(
            "── stats ── (provider={provider_id}, model={model}, n={n})\n"
        ));
    }
    out.push_str(&format!(
        "  TTFB     mean={:>7.0}ms   p50={:>7.0}ms   p99={:>7.0}ms\n",
        ttfb_ms.mean, ttfb_ms.p50, ttfb_ms.p99,
    ));
    out.push_str(&format!(
        "  Total    mean={:>7.2}s    p50={:>7.2}s    p99={:>7.2}s\n",
        total_s.mean, total_s.p50, total_s.p99,
    ));
    out.push_str(&format!(
        "  Decode   mean={:>7.1}     p50={:>7.1}     p99={:>7.1}     tok/s\n",
        decode.mean, decode.p50, decode.p99,
    ));
    out.push_str(&format!(
        "  Out      mean={out_mean:>7.1}                                  tokens\n"
    ));
    out.push_str(&format!(
        "  In       mean={in_mean:>7.1}                                  tokens\n"
    ));
    if *thinking_total > 0 {
        out.push_str(&format!(
            "  (Thinking tokens reported across all iterations: {thinking_total})\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ttfb_ms: u64, total_ms: u64, out: u64) -> Sample {
        Sample {
            first_token: Some(Duration::from_millis(ttfb_ms)),
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
        let sample = s(500, 500, 100);
        assert_eq!(sample.decode_tok_per_sec(), 0.0);
    }

    #[test]
    fn a_bench_whose_every_iteration_generated_nothing_measured_nothing() {
        let samples = [s(100, 200, 0), s(100, 200, 0)];
        assert_eq!(
            summarize(&samples),
            Summary::NothingGenerated { iterations: 2 }
        );
        let mut no_first_token = s(100, 200, 9);
        no_first_token.first_token = None;
        assert_eq!(
            summarize(&[no_first_token]),
            Summary::NothingGenerated { iterations: 1 }
        );
    }

    #[test]
    fn the_summary_leaves_out_iterations_that_generated_nothing() {
        let samples = [s(100, 1100, 100), s(300, 1300, 100), s(50, 60, 0)];
        let Summary::Measured {
            n,
            skipped,
            ttfb_ms,
            ..
        } = summarize(&samples)
        else {
            panic!("two iterations generated tokens");
        };
        assert_eq!((n, skipped), (2, 1));
        assert_eq!(
            ttfb_ms,
            Stats {
                mean: 200.0,
                p50: 300.0,
                p99: 300.0
            }
        );
    }

    #[tokio::test]
    async fn one_iteration_opens_one_call_span_for_the_benched_model() {
        use tars_llm::{CannedResponse, MockProvider};
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry::Registry;

        use crate::single_attempt::testkit::CallSpans;

        let spans = CallSpans::default();
        let _guard = tracing::subscriber::set_default(Registry::default().with(spans.clone()));
        let provider = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("hello"));
        let caller = Caller::new(
            TenantId::from_str("acme").expect("a plain tenant id parses"),
            tars_types::TraceId::from(uuid::Uuid::new_v4()),
        );

        run_one(
            &single_attempt::service(provider, "bench-model"),
            &caller,
            "hi",
            100,
        )
        .await
        .expect("the mock answers and finishes");

        assert_eq!(spans.models(), ["bench-model"]);
    }

    #[test]
    fn p99_is_the_largest_value_below_a_hundred_and_the_99th_at_a_hundred() {
        assert_eq!(Stats::of(5.0, []).p99, 5.0);
        let below = Stats::of(1.0, (2..=99).map(f64::from));
        assert_eq!(below.p99, 99.0);
        let hundred = Stats::of(1.0, (2..=100).map(f64::from));
        assert_eq!(hundred.p99, 99.0);
    }
}
