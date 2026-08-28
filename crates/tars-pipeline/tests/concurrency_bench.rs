//! Host-concurrency bench: how many in-flight LLM steps can one box hold when the
//! ONLY fake is the remote round-trip (a `sleep`)? Everything else is the real
//! path — real `ChatRequest`, real `LlmService` middleware onion, real stream drain.
//!
//! This isolates the orchestration machinery from the provider rate-limit ceiling:
//! an agent step is I/O-bound (parked on a remote async call), so the question is
//! whether the host can cheaply hold N such parked futures. `sleep` reproduces
//! exactly that park; a zero-latency mock would instead measure dispatch throughput.
//!
//! Run:  cargo test -p tars-pipeline --test concurrency_bench -- --nocapture

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{StreamExt, stream};

use tars_pipeline::{ChainOpts, LlmService};
use tars_provider::provider::{LlmEventStream, LlmProvider};
use tars_types::{
    ChatEvent, ChatRequest, Pricing, ProviderError, ProviderId, ProviderProfile, RequestContext,
    StopReason, Usage,
};

/// A provider whose ONLY departure from real is `sleep(latency)` in place of the
/// network round-trip. No shared mutex, no history recording — so the bench
/// measures the runtime, not the mock's own lock (the stock `MockProvider` takes a
/// global `Mutex` per call, which would serialize 10k callers on the mock itself).
struct LatencyMock {
    id: ProviderId,
    caps: ProviderProfile,
    latency: Duration,
}

impl LatencyMock {
    fn new(latency: Duration) -> Arc<Self> {
        let mut caps = ProviderProfile::text_only_baseline(Pricing::default());
        caps.interface = tars_types::InterfaceKind::Mock;
        Arc::new(Self {
            id: "latency-mock".into(),
            caps,
            latency,
        })
    }
}

#[async_trait]
impl LlmProvider for LatencyMock {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn capabilities(&self) -> &ProviderProfile {
        &self.caps
    }
    async fn stream(
        self: Arc<Self>,
        _req: ChatRequest,
        model: &str,
        _ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // The one fake: the remote async round-trip. This is the park point.
        tokio::time::sleep(self.latency).await;
        let events: Vec<Result<ChatEvent, ProviderError>> = vec![
            Ok(ChatEvent::started(model)),
            Ok(ChatEvent::Delta { text: "ok".into() }),
            Ok(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    // A realistic per-call token count so the rate limiter's
                    // usage-reconcile has something to bind on (a ~0-token mock
                    // would get fully refunded → no limiting).
                    output_tokens: 100,
                    ..Default::default()
                },
            }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

/// Drive `n` concurrent real `LlmService::call`s, each draining its stream. An
/// optional `Semaphore` gate models the tars-runtime executor's `max_concurrent`
/// cap (`tokio::sync::Semaphore`, executor.rs).
async fn drive(
    svc: Arc<LlmService>,
    n: usize,
    gate: Option<Arc<tokio::sync::Semaphore>>,
) -> Duration {
    let start = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let svc = svc.clone();
        let gate = gate.clone();
        handles.push(tokio::spawn(async move {
            let _permit = match &gate {
                Some(s) => Some(s.clone().acquire_owned().await.expect("sem")),
                None => None,
            };
            // Vary the prompt so a cache layer (if any) can't shadow the provider.
            let req = ChatRequest::user(format!("ping {i}"));
            let mut stream = svc
                .call(req, RequestContext::test_default())
                .await
                .expect("call");
            while let Some(ev) = stream.next().await {
                ev.expect("event");
            }
        }));
    }
    for h in handles {
        h.await.expect("join");
    }
    start.elapsed()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_concurrency_bench() {
    let latency = Duration::from_millis(50);

    // "Everything real except sleep": the full default middleware onion
    // (telemetry / retry / …) over the latency mock. cache=false so every call
    // hits the provider (a same-prompt cache hit would shadow the sleep).
    let mut opts = ChainOpts::new("latency-mock".into());
    opts.cache = false;
    let chained = Arc::new(LlmService::default_chain(
        LatencyMock::new(latency),
        "mock-model",
        opts,
    ));
    // Bare leaf (no middleware) to isolate the onion's per-call overhead.
    let bare = Arc::new(LlmService::of(LatencyMock::new(latency), "mock-model"));

    eprintln!("\n=== host-concurrency bench (latency/call = {latency:?}) ===");
    eprintln!("serialized baseline would be N * {latency:?}\n");
    eprintln!(
        "{:<10} {:<14} {:>10} {:>14} {:>12}",
        "service", "gate", "N", "wall", "throughput"
    );

    for &n in &[1_000usize, 10_000, 50_000] {
        let el = drive(chained.clone(), n, None).await;
        let tput = n as f64 / el.as_secs_f64();
        eprintln!(
            "{:<10} {:<14} {:>10} {:>14?} {:>10.0}/s",
            "chain", "unbounded", n, el, tput
        );
    }

    // The concurrency gate tars actually has: a tokio Semaphore (executor's
    // max_concurrent). N=10k offered, only C run at once → wall ≈ (N/C)*latency.
    for &c in &[500usize, 2_000] {
        let gate = Arc::new(tokio::sync::Semaphore::new(c));
        let n = 10_000;
        let el = drive(chained.clone(), n, Some(gate)).await;
        let tput = n as f64 / el.as_secs_f64();
        eprintln!(
            "{:<10} {:<14} {:>10} {:>14?} {:>10.0}/s",
            "chain",
            format!("sem({c})"),
            n,
            el,
            tput
        );
    }

    // Bare-vs-chain at 10k to show the middleware onion's overhead.
    let el = drive(bare.clone(), 10_000, None).await;
    let tput = 10_000f64 / el.as_secs_f64();
    eprintln!(
        "{:<10} {:<14} {:>10} {:>14?} {:>10.0}/s",
        "bare", "unbounded", 10_000, el, tput
    );

    // Proof of real concurrency: 10k calls at 50ms each, unbounded, must finish in
    // FAR less than the serialized 500s — i.e. they were genuinely parked together.
    let el = drive(chained.clone(), 10_000, None).await;
    assert!(
        el < Duration::from_secs(5),
        "10k concurrent 50ms calls took {el:?} — expected << serialized 500s; concurrency broken"
    );
    eprintln!(
        "\nconcurrency proof: 10k × 50ms unbounded finished in {el:?} (serialized would be 500s)\n"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// TPM limiter curve — drives the REAL `tars_pipeline::TpmRateLimiter` (proactive
// provider-rate backpressure), the production middleware. The mock reports 100
// output tokens per call, so the limiter's usage-reconcile binds on ~100
// tokens/call; with a TPM ceiling that's a sustained calls/s = TPM/60/100.
// ─────────────────────────────────────────────────────────────────────────────

use tars_pipeline::{TpmRateLimitConfig, TpmRateLimiter};

/// Build a service = the real TPM limiter over the bare latency mock. Small burst
/// (1000 tok ≈ 10 calls) so a short bench reads the sustained rate, not the spike.
fn limited(tpm: u32, latency: Duration) -> Arc<LlmService> {
    // Burst 6000 tok (~60 calls): above one 10ms tick's refill even at 30M TPM
    // (500k tok/s × 10ms = 5000), so the bucket sustains the rate; and small
    // relative to the 200k-token run, so the curve reads steady-state not spike.
    let cfg = TpmRateLimitConfig {
        tokens_per_minute: tpm,
        burst_tokens: Some(6_000),
        reserved_output_tokens: 100,
    };
    Arc::new(
        LlmService::builder(LatencyMock::new(latency), "mock-model")
            .layer(TpmRateLimiter::new(cfg))
            .build(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tpm_limiter_curve() {
    // Small latency so the LIMITER is the binding constraint, not the mock sleep.
    let latency = Duration::from_millis(5);
    let cost = 100.0; // tokens/call the mock reports → the limiter binds on this
    let n = 2_000usize; // offered tokens = n * cost = 200k

    eprintln!(
        "\n=== TPM limiter curve — real TpmRateLimiter (latency={latency:?}, ~{cost}tok/call, N={n}) ==="
    );
    eprintln!(
        "{:>14} {:>12} {:>14} {:>14} {:>12}",
        "configured TPM", "wall", "achieved TPM", "achieved/cfg", "calls/s"
    );

    for &tpm in &[3_000_000u32, 6_000_000, 15_000_000, 30_000_000] {
        let el = drive(limited(tpm, latency), n, None).await;
        let achieved_tpm = (n as f64 * cost) / el.as_secs_f64() * 60.0;
        let calls_s = n as f64 / el.as_secs_f64();
        eprintln!(
            "{:>14} {:>12?} {:>14.0} {:>13.0}% {:>12.0}",
            tpm,
            el,
            achieved_tpm,
            achieved_tpm / tpm as f64 * 100.0,
            calls_s
        );
    }

    // No limiter: the ceiling this box hits without any rate gate.
    let el = drive(
        Arc::new(LlmService::of(LatencyMock::new(latency), "mock-model")),
        n,
        None,
    )
    .await;
    eprintln!(
        "{:>14} {:>12?} {:>14} {:>14} {:>12.0}",
        "none",
        el,
        "-",
        "-",
        n as f64 / el.as_secs_f64()
    );

    // The curve must track: a tighter TPM must yield a strictly longer wall.
    let tight = drive(limited(3_000_000, latency), n, None).await;
    let loose = drive(limited(30_000_000, latency), n, None).await;
    assert!(
        tight > loose,
        "tighter TPM (3M) wall {tight:?} should exceed looser (30M) {loose:?} — limiter not binding"
    );
    eprintln!("\nmonotonicity: 3M-TPM wall {tight:?} > 30M-TPM wall {loose:?} ✓\n");
}

/// Drive `n` calls split evenly across two services, concurrently; return the
/// combined wall time.
async fn drive_split(a: Arc<LlmService>, b: Arc<LlmService>, n: usize) -> Duration {
    let start = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let svc = if i % 2 == 0 { a.clone() } else { b.clone() };
        handles.push(tokio::spawn(async move {
            let mut s = svc
                .call(
                    ChatRequest::user(format!("ping {i}")),
                    RequestContext::test_default(),
                )
                .await
                .expect("call");
            while let Some(ev) = s.next().await {
                ev.expect("event");
            }
        }));
    }
    for h in handles {
        h.await.expect("join");
    }
    start.elapsed()
}

/// Two services over ONE shared account bucket must deliver a COMBINED rate
/// of ~1×TPM; two services with PRIVATE buckets deliver ~2×TPM. This proves
/// that `shared()` correctly throttles across instances.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_bucket_binds_combined_rate() {
    let latency = Duration::from_millis(5);
    let cost = 100.0;
    let tpm = 6_000_000u32; // 100k tok/s
    let n = 2_000usize; // 200k tokens total across BOTH services
    let cfg = || TpmRateLimitConfig {
        tokens_per_minute: tpm,
        burst_tokens: Some(6_000),
        reserved_output_tokens: 100,
    };
    let build = |lim: TpmRateLimiter| {
        Arc::new(
            LlmService::builder(LatencyMock::new(latency), "m")
                .layer(lim)
                .build(),
        )
    };

    // Two services, ONE shared account bucket.
    let shared_wall = drive_split(
        build(TpmRateLimiter::shared("bench-acct", cfg())),
        build(TpmRateLimiter::shared("bench-acct", cfg())),
        n,
    )
    .await;
    // Two services, PRIVATE buckets.
    let private_wall = drive_split(
        build(TpmRateLimiter::new(cfg())),
        build(TpmRateLimiter::new(cfg())),
        n,
    )
    .await;

    let shared_tpm = (n as f64 * cost) / shared_wall.as_secs_f64() * 60.0;
    let private_tpm = (n as f64 * cost) / private_wall.as_secs_f64() * 60.0;
    eprintln!(
        "\n=== shared vs private bucket (2 services, configured {tpm} TPM) ===\n\
         shared  bucket: {shared_wall:?}  combined {shared_tpm:.0} TPM (~1×)\n\
         private bucket: {private_wall:?}  combined {private_tpm:.0} TPM (~2×, the bug)\n"
    );

    assert!(
        shared_tpm < tpm as f64 * 1.25,
        "shared combined {shared_tpm:.0} must be ~1×TPM {tpm}, not N×"
    );
    assert!(
        private_tpm > tpm as f64 * 1.6,
        "private combined {private_tpm:.0} must be ~2×TPM (demonstrates the pre-fix blow-out)"
    );
}
