//! Measures how many in-flight calls one host can hold when the only fake is the remote
//! round-trip, a `sleep`; the request, the middleware chain and the stream drain are real. A
//! `sleep` reproduces a call parked on the network; a zero-latency mock would measure dispatch
//! throughput instead.
//!
//! Run:  cargo test -p tars-llm --test concurrency_bench -- --nocapture

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{StreamExt, stream};

use tars_llm::ProviderError;
use tars_llm::provider::{LlmEventStream, LlmProvider};
use tars_llm::{ChainOptions, LlmService};
use tars_types::{
    Caller, ChatEvent, ChatRequest, Pricing, ProviderId, ProviderProfile, StopReason, Usage,
};

/// Sleeps in place of the network round-trip and takes no lock: `MockProvider` locks one
/// `Mutex` per call, which would serialize 10k callers on the mock itself.
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
            id: ProviderId::new("latency-mock"),
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
        _ctx: Caller,
    ) -> Result<LlmEventStream, ProviderError> {
        tokio::time::sleep(self.latency).await;
        let events: Vec<Result<ChatEvent, ProviderError>> = vec![
            Ok(ChatEvent::started(model)),
            Ok(ChatEvent::Delta { text: "ok".into() }),
            Ok(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Some(Usage {
                    // A zero-token call would be refunded in full, and the limiter would not bind.
                    output_tokens: 100,
                    ..Default::default()
                }),
            }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

/// Runs `n` concurrent calls, each draining its stream, at most `gate`'s permits at once, and
/// returns the wall time.
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
            let req = ChatRequest::user(format!("ping {i}"));
            let mut stream = svc
                .call(req, Caller::test_default())
                .await
                .result
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

    let opts = ChainOptions::default();
    let chained = Arc::new(LlmService::default_chain(
        LatencyMock::new(latency),
        "mock-model",
        opts,
    ));
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

    // With only C running at once, wall ≈ (N / C) × latency.
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

    let el = drive(bare.clone(), 10_000, None).await;
    let tput = 10_000f64 / el.as_secs_f64();
    eprintln!(
        "{:<10} {:<14} {:>10} {:>14?} {:>10.0}/s",
        "bare", "unbounded", 10_000, el, tput
    );

    // Run one after another, 10k calls of 50 ms take 500 s.
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
// TPM limiter curve. The mock reports 100 output tokens per call, so the limiter sustains
// TPM / 60 / 100 calls a second.
// ─────────────────────────────────────────────────────────────────────────────

use std::num::NonZeroU32;
use tars_llm::{TpmBuckets, TpmRateLimitConfig, TpmRateLimiter};

/// Returns the TPM limiter over the latency mock.
fn limited(tpm: u32, latency: Duration) -> Arc<LlmService> {
    // A 6000-token burst is above one 10 ms tick's refill even at 30M TPM (5000), so the bucket
    // sustains the rate, and small against the 200k-token run, so the bench reads the sustained
    // rate rather than the burst.
    let cfg = TpmRateLimitConfig {
        tokens_per_minute: NonZeroU32::new(tpm).expect("the bench's TPM values are positive"),
        burst_tokens: NonZeroU32::new(6_000),
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
    // Small, so the limiter rather than the sleep bounds the run.
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

/// Runs `n` calls split evenly across two services at once and returns the wall time.
async fn drive_split(a: Arc<LlmService>, b: Arc<LlmService>, n: usize) -> Duration {
    let start = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let svc = if i % 2 == 0 { a.clone() } else { b.clone() };
        handles.push(tokio::spawn(async move {
            let mut s = svc
                .call(
                    ChatRequest::user(format!("ping {i}")),
                    Caller::test_default(),
                )
                .await
                .result
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

/// Two services over one shared bucket deliver about 1 × TPM together; with private buckets
/// they deliver about 2 × TPM.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_bucket_binds_combined_rate() {
    let latency = Duration::from_millis(5);
    let cost = 100.0;
    let tpm = 6_000_000u32; // 100k tok/s
    let n = 2_000usize; // 200k tokens total across BOTH services
    let cfg = || TpmRateLimitConfig {
        tokens_per_minute: NonZeroU32::new(tpm).expect("the bench's TPM values are positive"),
        burst_tokens: NonZeroU32::new(6_000),
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
    let buckets = TpmBuckets::new();
    let shared_wall = drive_split(
        build(buckets.limiter("bench-acct", cfg()).unwrap()),
        build(buckets.limiter("bench-acct", cfg()).unwrap()),
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
