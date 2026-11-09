//! Stops calling a provider that keeps failing, and lets one probe through after a cooldown.
//!
//! The breaker wraps the provider, so in a chain it sits below Retry. It opens after
//! `failure_threshold` consecutive health failures and resets the count on a success; there is no
//! sliding window. A health failure is the provider being down or broken: a network error, a
//! server error or overload, a timeout, a CLI backend dying, or a response that does not parse.
//! Every other error neither counts nor resets: the caller's own request, quota, content or tool
//! use says nothing about the provider, and an `Internal` error is a bug in tars that a breaker
//! cannot route around. While open it rejects with [`ProviderError::Unavailable`], which says until
//! when and carries the failure that opened it; it is `Transient`, and what to do with it (wait,
//! or move to another provider) is the caller's. After the cooldown the next call is the one probe:
//! success closes the breaker, failure opens it for a fresh cooldown.
//!
//! `stream` and `complete` judge a call by the same end: it succeeds when the provider says the
//! answer is complete (a `Finished` event, a response with a stop reason) and fails on the first
//! error, whether the stream had opened or not. A stream that ends without either, or that the
//! consumer drops, leaves no verdict. An outcome counts only in the closed period that let its
//! call through: each opening starts a new generation, so a slow call from before an opening
//! moves neither the open breaker nor the closed one after it.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::{LastFailure, ProviderError};
use crate::{LlmEventStream, LlmProvider};
use tars_types::{
    Caller, ChatEvent, ChatRequest, ChatResponse, CostUsd, LlmErrorKind, ProviderId,
    ProviderProfile, Usage,
};

/// Configures a [`CircuitBreaker`]. The default opens after 5 failures and cools down for 30 s.
/// It reads from the `[llm.circuit_breaker]` config section (`failure_threshold`,
/// `cooldown_secs`) through [`CircuitBreakerConfig::new`]; a key left out keeps its default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "BreakerKeys", into = "BreakerKeys")]
pub struct CircuitBreakerConfig {
    failure_threshold: NonZeroU32,
    cooldown: Duration,
}

const DEFAULT_FAILURE_THRESHOLD: NonZeroU32 = NonZeroU32::new(5).unwrap();

/// Is the longest cooldown [`CircuitBreakerConfig::new`] accepts. A breaker that waits longer
/// than a day before probing has turned its provider off, which is not what a cooldown is for;
/// the bound also keeps `Instant + cooldown` far inside what any clock can represent.
pub const MAX_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);

impl CircuitBreakerConfig {
    /// Returns a config that opens after `failure_threshold` consecutive health failures and
    /// stays open for `cooldown`.
    ///
    /// # Errors
    ///
    /// Returns [`CooldownTooLongError`] when `cooldown` exceeds [`MAX_COOLDOWN`].
    pub fn new(
        failure_threshold: NonZeroU32,
        cooldown: Duration,
    ) -> Result<Self, CooldownTooLongError> {
        if cooldown > MAX_COOLDOWN {
            return Err(CooldownTooLongError { cooldown });
        }
        Ok(Self {
            failure_threshold,
            cooldown,
        })
    }
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            cooldown: Duration::from_secs(30),
        }
    }
}

/// The `[llm.circuit_breaker]` keys as written, before [`CircuitBreakerConfig::new`] checks them.
#[derive(Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BreakerKeys {
    failure_threshold: NonZeroU32,
    #[serde(rename = "cooldown_secs", with = "crate::duration_serde::secs")]
    cooldown: Duration,
}

impl Default for BreakerKeys {
    fn default() -> Self {
        CircuitBreakerConfig::default().into()
    }
}

impl From<CircuitBreakerConfig> for BreakerKeys {
    fn from(c: CircuitBreakerConfig) -> Self {
        Self {
            failure_threshold: c.failure_threshold,
            cooldown: c.cooldown,
        }
    }
}

impl TryFrom<BreakerKeys> for CircuitBreakerConfig {
    type Error = CooldownTooLongError;

    fn try_from(k: BreakerKeys) -> Result<Self, Self::Error> {
        Self::new(k.failure_threshold, k.cooldown)
    }
}

/// Says a circuit breaker's cooldown is longer than [`MAX_COOLDOWN`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("circuit breaker cooldown {cooldown:?} exceeds the maximum of {MAX_COOLDOWN:?}")]
pub struct CooldownTooLongError {
    pub cooldown: Duration,
}

/// How long a caller that arrives while the probe is in flight is told to wait.
const PROBE_BUSY_WAIT: Duration = Duration::from_millis(100);

/// Returns `now + wait` for a wait no longer than [`MAX_COOLDOWN`].
fn after(now: Instant, wait: Duration) -> Instant {
    now.checked_add(wait).expect(
        "every wait here is at most MAX_COOLDOWN (one day), and no platform clock is within a \
         day of the end of its range",
    )
}

/// The breaker's phase and how many times it has opened.
#[derive(Clone, Debug, Eq, PartialEq)]
struct BreakerState {
    /// Grows by one each time the breaker opens, so a call let through in an earlier closed
    /// period is told apart from one of the current period. Only equality is compared, so a wrap
    /// after 2^64 openings is harmless.
    generation: u64,
    phase: Phase,
}

/// `last_failure` is the failure that opened the breaker, kept until it closes again.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Phase {
    Closed {
        failures: u32,
    },
    Open {
        until: Instant,
        last_failure: LastFailure,
    },
    /// `probe_in_flight` keeps callers racing the cooldown's end from all reaching the provider;
    /// it is false again after a probe was dropped without an outcome.
    HalfOpen {
        probe_in_flight: bool,
        last_failure: LastFailure,
    },
}

impl BreakerState {
    const CLOSED: Self = Self {
        generation: 0,
        phase: Phase::Closed { failures: 0 },
    };

    fn with(&self, phase: Phase) -> Self {
        Self {
            phase,
            generation: self.generation,
        }
    }

    fn opened(&self, until: Instant, last_failure: LastFailure) -> Self {
        Self {
            generation: self.generation.wrapping_add(1),
            phase: Phase::Open {
                until,
                last_failure,
            },
        }
    }
}

/// Which state let a call through, so its outcome is weighed only there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Admission {
    /// Let through while closed, in this generation.
    Ordinary { generation: u64 },
    /// The one probe of a half-open breaker. It needs no generation: while it is in flight
    /// nothing else moves the breaker out of half-open.
    Probe,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Decision {
    Allow(Admission),
    Reject {
        until: Instant,
        last_failure: LastFailure,
    },
}

/// What one call says about the provider's health.
#[derive(Clone, Debug, Eq, PartialEq)]
enum HealthVerdict {
    Success,
    Failure(LastFailure),
    /// The call failed for a reason that says nothing about the provider's health (the caller's
    /// request, quota or content, the model's own malformed tool call, a reply cut off at the
    /// caller's output limit, or a bug in our adapter),
    /// or it ended without a verdict.
    Ignored,
}

/// Returns what a failed call says about the provider's health.
fn outcome_of(e: &ProviderError) -> HealthVerdict {
    if says_provider_is_down(e.kind()) {
        HealthVerdict::Failure(LastFailure::of(e))
    } else {
        HealthVerdict::Ignored
    }
}

/// Returns whether an error of `kind` says the provider is down or broken.
fn says_provider_is_down(kind: LlmErrorKind) -> bool {
    use LlmErrorKind as K;
    match kind {
        K::Network
        | K::ModelOverloaded
        | K::ServerError
        | K::TimedOut
        | K::CliSubprocessDied
        | K::Parse => true,
        K::Auth
        | K::RequestTimeout
        | K::ProviderTimedOut
        | K::StreamCut
        | K::Unfinished
        | K::StreamViolation
        | K::RateLimited
        | K::BudgetExceeded
        | K::InvalidRequest
        | K::ContentFiltered
        | K::ContextTooLong
        | K::Unavailable
        | K::MalformedToolCall
        | K::TruncatedToolCall
        | K::IncompleteToolCall
        | K::Incompatible
        | K::ValidationFailed
        | K::ValidationCancelled
        | K::BudgetStoreFailed
        | K::BudgetBalanceCorrupt
        | K::Cancelled
        | K::Encode
        | K::InvalidUrl
        | K::InvalidHeader
        | K::DaemonScript
        | K::Spawn
        | K::Io
        | K::DaemonFailed
        | K::DaemonExited
        | K::Sandbox
        | K::BatchItemCanceled
        | K::BatchItemExpired
        | K::BatchItemFailed
        | K::Internal => false,
    }
}

/// Returns the outcome a stream item settles the call with, or `None` when the call goes on.
fn terminal_outcome(item: &Result<ChatEvent, ProviderError>) -> Option<HealthVerdict> {
    match item {
        Ok(ChatEvent::Finished { .. }) => Some(HealthVerdict::Success),
        Ok(_) => None,
        Err(e) => Some(outcome_of(e)),
    }
}

/// Decides whether a call arriving at `now` goes through, and the state after it.
fn admit(state: &BreakerState, now: Instant) -> (BreakerState, Decision) {
    match &state.phase {
        Phase::Closed { .. } => (
            state.clone(),
            Decision::Allow(Admission::Ordinary {
                generation: state.generation,
            }),
        ),
        Phase::Open {
            until,
            last_failure,
        } if now < *until => (
            state.clone(),
            Decision::Reject {
                until: *until,
                last_failure: last_failure.clone(),
            },
        ),
        Phase::Open { last_failure, .. }
        | Phase::HalfOpen {
            probe_in_flight: false,
            last_failure,
        } => (
            state.with(Phase::HalfOpen {
                probe_in_flight: true,
                last_failure: last_failure.clone(),
            }),
            Decision::Allow(Admission::Probe),
        ),
        Phase::HalfOpen {
            probe_in_flight: true,
            last_failure,
        } => (
            state.clone(),
            Decision::Reject {
                until: after(now, PROBE_BUSY_WAIT),
                last_failure: last_failure.clone(),
            },
        ),
    }
}

/// Returns the state after a call let through as `admission` ends with `outcome` at `now`.
fn settle(
    config: CircuitBreakerConfig,
    state: &BreakerState,
    admission: Admission,
    outcome: HealthVerdict,
    now: Instant,
) -> BreakerState {
    let open = |last_failure| state.opened(after(now, config.cooldown), last_failure);
    match (&state.phase, admission, outcome) {
        (_, Admission::Ordinary { generation }, _) if generation != state.generation => {
            state.clone()
        }
        (Phase::Closed { .. }, Admission::Ordinary { .. }, HealthVerdict::Success) => {
            state.with(Phase::Closed { failures: 0 })
        }
        (Phase::Closed { failures }, Admission::Ordinary { .. }, HealthVerdict::Failure(f)) => {
            let failures = failures.saturating_add(1);
            if failures >= config.failure_threshold.get() {
                open(f)
            } else {
                state.with(Phase::Closed { failures })
            }
        }
        (Phase::HalfOpen { .. }, Admission::Probe, HealthVerdict::Success) => {
            state.with(Phase::Closed { failures: 0 })
        }
        (Phase::HalfOpen { .. }, Admission::Probe, HealthVerdict::Failure(f)) => open(f),
        (Phase::HalfOpen { last_failure, .. }, Admission::Probe, HealthVerdict::Ignored) => state
            .with(Phase::HalfOpen {
                probe_in_flight: false,
                last_failure: last_failure.clone(),
            }),
        (Phase::Closed { .. }, Admission::Ordinary { .. }, HealthVerdict::Ignored)
        | (Phase::Open { .. } | Phase::HalfOpen { .. }, Admission::Ordinary { .. }, _)
        | (Phase::Closed { .. } | Phase::Open { .. }, Admission::Probe, _) => state.clone(),
    }
}

/// A provider wrapped in a circuit breaker. The state lives on the wrapper, so wrapping one
/// provider twice gives two independent breakers; wrap it once, where the registry is built.
pub struct CircuitBreaker {
    inner: Arc<dyn LlmProvider>,
    id: ProviderId,
    config: CircuitBreakerConfig,
    state: Mutex<BreakerState>,
}

impl CircuitBreaker {
    /// Wraps `provider` in a closed breaker.
    pub fn wrap(
        provider: Arc<dyn LlmProvider>,
        config: CircuitBreakerConfig,
    ) -> Arc<dyn LlmProvider> {
        Self::closed(provider, config)
    }

    fn closed(provider: Arc<dyn LlmProvider>, config: CircuitBreakerConfig) -> Arc<Self> {
        let id = provider.id().clone();
        Arc::new(Self {
            inner: provider,
            id,
            config,
            state: Mutex::new(BreakerState::CLOSED),
        })
    }

    /// Locks the state, recovering a poisoned lock: each holder computes the next state before it
    /// assigns it, so a panic while holding the lock leaves the previous state whole.
    fn lock_state(&self) -> MutexGuard<'_, BreakerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lets a call through and returns the [`Settlement`] it must end with, or the instant to
    /// come back at.
    fn enter(self: &Arc<Self>) -> Result<Settlement, ProviderError> {
        let now = Instant::now();
        let mut state = self.lock_state();
        let (next, decision) = admit(&state, now);
        log_transition(&self.id, &state, &next);
        *state = next;
        drop(state);
        match decision {
            Decision::Allow(admission) => Ok(Settlement {
                breaker: Some(self.clone()),
                admission,
            }),
            Decision::Reject {
                until,
                last_failure,
            } => {
                tracing::debug!(
                    provider_id = %self.id,
                    until_ms = until.saturating_duration_since(now).as_millis() as u64,
                    "circuit_breaker: rejecting",
                );
                Err(ProviderError::Unavailable {
                    until,
                    last_failure,
                })
            }
        }
    }

    fn record(&self, admission: Admission, outcome: HealthVerdict) {
        let mut state = self.lock_state();
        let next = settle(self.config, &state, admission, outcome, Instant::now());
        log_transition(&self.id, &state, &next);
        *state = next;
    }

    #[cfg(test)]
    fn current(&self) -> BreakerState {
        self.lock_state().clone()
    }
}

fn log_transition(id: &ProviderId, before: &BreakerState, after: &BreakerState) {
    use Phase::{Closed, HalfOpen, Open};
    match (&before.phase, &after.phase) {
        (Closed { failures }, Open { last_failure, .. }) => tracing::warn!(
            provider_id = %id,
            consecutive_failures = failures.saturating_add(1),
            last_failure_kind = %last_failure.kind,
            last_failure = %last_failure.message,
            "circuit_breaker: failure threshold reached → Open",
        ),
        (HalfOpen { .. }, Open { last_failure, .. }) => tracing::warn!(
            provider_id = %id,
            last_failure_kind = %last_failure.kind,
            last_failure = %last_failure.message,
            "circuit_breaker: probe failed → Open (fresh cooldown)",
        ),
        (HalfOpen { .. }, Closed { .. }) => tracing::info!(
            provider_id = %id,
            "circuit_breaker: probe succeeded → Closed",
        ),
        (
            HalfOpen {
                probe_in_flight: true,
                ..
            },
            HalfOpen {
                probe_in_flight: false,
                ..
            },
        ) => tracing::warn!(
            provider_id = %id,
            "circuit_breaker: probe ended without a verdict; released probe slot",
        ),
        _ => {}
    }
}

/// The obligation of one call let through to report how it ended. Dropping it unsettled (a
/// timeout, a disconnect, a consumer that stopped reading) records [`HealthVerdict::Ignored`],
/// which frees a probe's slot; without that the slot stays taken and every later call is
/// rejected.
struct Settlement {
    breaker: Option<Arc<CircuitBreaker>>,
    admission: Admission,
}

impl Settlement {
    fn settle(mut self, outcome: HealthVerdict) {
        if let Some(breaker) = self.breaker.take() {
            breaker.record(self.admission, outcome);
        }
    }
}

impl Drop for Settlement {
    fn drop(&mut self) {
        if let Some(breaker) = self.breaker.take() {
            breaker.record(self.admission, HealthVerdict::Ignored);
        }
    }
}

#[async_trait]
impl LlmProvider for CircuitBreaker {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn capabilities(&self) -> &ProviderProfile {
        self.inner.capabilities()
    }
    fn output_limit(&self, model: &str) -> tars_types::OutputLimit {
        self.inner.output_limit(model)
    }

    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<LlmEventStream, ProviderError> {
        let settlement = self.enter()?;
        let inner = match self.inner.clone().stream(req, model, ctx).await {
            Ok(inner) => inner,
            Err(e) => {
                settlement.settle(outcome_of(&e));
                return Err(e);
            }
        };
        let observed = async_stream::stream! {
            let mut settlement = Some(settlement);
            let mut inner = inner;
            while let Some(item) = inner.next().await {
                if let Some(outcome) = terminal_outcome(&item)
                    && let Some(s) = settlement.take()
                {
                    s.settle(outcome);
                }
                yield item;
            }
            if let Some(s) = settlement.take() {
                s.settle(HealthVerdict::Ignored);
            }
        };
        Ok(Box::pin(observed))
    }

    async fn complete(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<ChatResponse, ProviderError> {
        // Calls the inner `complete` rather than the trait default, which would drain
        // `self.stream`, so an inner provider's own `complete` is kept.
        let settlement = self.enter()?;
        let result = self.inner.clone().complete(req, model, ctx).await;
        settlement.settle(match &result {
            Ok(_) => HealthVerdict::Success,
            Err(e) => outcome_of(e),
        });
        result
    }

    fn cost(&self, usage: &Usage) -> CostUsd {
        self.inner.cost(usage)
    }

    async fn count_tokens(&self, req: &ChatRequest, fast: bool) -> Result<u64, ProviderError> {
        self.inner.count_tokens(req, fast).await
    }

    /// Returns the inner provider's batch API. A batch is not a call the breaker watches: it is
    /// submitted and polled outside the request path, so it neither waits on nor moves the breaker.
    fn as_batch_submitter(
        self: Arc<Self>,
    ) -> Option<Arc<dyn crate::provider::batch::BatchSubmitter>> {
        self.inner.clone().as_batch_submitter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::error::{ParseProblem, RequestRefusal};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tars_types::StopReason;

    use tokio::sync::mpsc;

    const COOLDOWN: Duration = Duration::from_secs(30);

    fn config(failure_threshold: u32) -> CircuitBreakerConfig {
        CircuitBreakerConfig::new(NonZeroU32::new(failure_threshold).unwrap(), COOLDOWN).unwrap()
    }

    fn at(generation: u64, phase: Phase) -> BreakerState {
        BreakerState { generation, phase }
    }

    const ORDINARY: Admission = Admission::Ordinary { generation: 0 };

    fn down() -> LastFailure {
        LastFailure {
            kind: LlmErrorKind::ModelOverloaded,
            message: "model overloaded".into(),
        }
    }

    // ── config ───────────────────────────────────────────────────────

    #[test]
    fn a_cooldown_of_the_maximum_is_accepted_and_one_beyond_it_is_refused() {
        assert!(CircuitBreakerConfig::new(NonZeroU32::MIN, MAX_COOLDOWN).is_ok());
        let beyond = MAX_COOLDOWN + Duration::from_nanos(1);
        assert_eq!(
            CircuitBreakerConfig::new(NonZeroU32::MIN, beyond),
            Err(CooldownTooLongError { cooldown: beyond })
        );
    }

    // ── outcome_of / terminal_outcome ────────────────────────────────

    #[test]
    fn provider_health_errors_are_failures() {
        for kind in [
            LlmErrorKind::Network,
            LlmErrorKind::ModelOverloaded,
            LlmErrorKind::ServerError,
            LlmErrorKind::TimedOut,
            LlmErrorKind::CliSubprocessDied,
            LlmErrorKind::Parse,
        ] {
            assert!(says_provider_is_down(kind), "{kind}");
        }
    }

    #[test]
    fn errors_that_say_nothing_about_the_providers_health_are_ignored() {
        for kind in [
            LlmErrorKind::Auth,
            LlmErrorKind::RateLimited,
            LlmErrorKind::InvalidRequest,
            LlmErrorKind::ContentFiltered,
            LlmErrorKind::ContextTooLong,
            LlmErrorKind::BudgetExceeded,
            LlmErrorKind::MalformedToolCall,
            LlmErrorKind::TruncatedToolCall,
            LlmErrorKind::IncompleteToolCall,
            LlmErrorKind::StreamViolation,
            LlmErrorKind::Incompatible,
            LlmErrorKind::ValidationFailed,
            LlmErrorKind::Unavailable,
            LlmErrorKind::Internal,
        ] {
            assert!(!says_provider_is_down(kind), "{kind}");
        }
    }

    #[test]
    fn a_stream_settles_on_finished_or_on_an_error_and_not_before() {
        let finished = Ok(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Some(Usage::default()),
        });
        assert_eq!(terminal_outcome(&finished), Some(HealthVerdict::Success));
        assert_eq!(
            terminal_outcome(&Ok(ChatEvent::Delta { text: "x".into() })),
            None
        );
        assert_eq!(
            terminal_outcome(&Err(ProviderError::ModelOverloaded)),
            Some(HealthVerdict::Failure(down()))
        );
        assert_eq!(
            terminal_outcome(&Err(ProviderError::InvalidRequest(
                RequestRefusal::EmptyMessages
            ))),
            Some(HealthVerdict::Ignored)
        );
    }

    #[test]
    fn a_stream_that_ended_without_saying_why_has_no_verdict() {
        assert_eq!(
            outcome_of(&ProviderError::Unfinished(
                tars_types::Unfinished::NoStopReason
            )),
            HealthVerdict::Ignored
        );
    }

    // ── admit ────────────────────────────────────────────────────────

    #[test]
    fn closed_lets_every_call_through_in_its_generation() {
        let now = Instant::now();
        let s = at(7, Phase::Closed { failures: 3 });
        assert_eq!(
            admit(&s, now),
            (s, Decision::Allow(Admission::Ordinary { generation: 7 }))
        );
    }

    #[test]
    fn open_rejects_until_the_cooldown_ends_then_lets_one_probe_through() {
        let now = Instant::now();
        let until = now + Duration::from_millis(1);
        let open = at(
            1,
            Phase::Open {
                until,
                last_failure: down(),
            },
        );
        assert_eq!(
            admit(&open, now),
            (
                open.clone(),
                Decision::Reject {
                    until,
                    last_failure: down()
                }
            )
        );
        assert_eq!(
            admit(&open, until),
            (
                at(
                    1,
                    Phase::HalfOpen {
                        probe_in_flight: true,
                        last_failure: down()
                    }
                ),
                Decision::Allow(Admission::Probe)
            )
        );
    }

    #[test]
    fn half_open_rejects_while_the_probe_is_in_flight_and_admits_a_new_probe_after() {
        let now = Instant::now();
        let busy = at(
            1,
            Phase::HalfOpen {
                probe_in_flight: true,
                last_failure: down(),
            },
        );
        assert_eq!(
            admit(&busy, now),
            (
                busy.clone(),
                Decision::Reject {
                    until: now + PROBE_BUSY_WAIT,
                    last_failure: down()
                }
            )
        );
        let free = at(
            1,
            Phase::HalfOpen {
                probe_in_flight: false,
                last_failure: down(),
            },
        );
        assert_eq!(admit(&free, now), (busy, Decision::Allow(Admission::Probe)));
    }

    // ── settle ───────────────────────────────────────────────────────

    #[test]
    fn closed_opens_on_the_threshold_failure_and_not_one_before() {
        let now = Instant::now();
        let one_short = settle(
            config(3),
            &at(0, Phase::Closed { failures: 1 }),
            ORDINARY,
            HealthVerdict::Failure(down()),
            now,
        );
        assert_eq!(one_short, at(0, Phase::Closed { failures: 2 }));
        let at_threshold = settle(
            config(3),
            &one_short,
            ORDINARY,
            HealthVerdict::Failure(down()),
            now,
        );
        assert_eq!(
            at_threshold,
            at(
                1,
                Phase::Open {
                    until: now + COOLDOWN,
                    last_failure: down()
                }
            )
        );
    }

    #[test]
    fn success_resets_the_failure_count_and_an_ignored_outcome_leaves_it() {
        let now = Instant::now();
        let s = at(0, Phase::Closed { failures: 2 });
        assert_eq!(
            settle(config(3), &s, ORDINARY, HealthVerdict::Success, now),
            at(0, Phase::Closed { failures: 0 })
        );
        assert_eq!(
            settle(config(3), &s, ORDINARY, HealthVerdict::Ignored, now),
            s
        );
    }

    #[test]
    fn an_outcome_counts_in_its_own_generation_and_not_in_the_next() {
        let now = Instant::now();
        let closed_again = at(1, Phase::Closed { failures: 0 });
        let current = Admission::Ordinary { generation: 1 };
        let earlier = Admission::Ordinary { generation: 0 };
        assert!(matches!(
            settle(
                config(1),
                &closed_again,
                current,
                HealthVerdict::Failure(down()),
                now
            )
            .phase,
            Phase::Open { .. }
        ));
        for outcome in [HealthVerdict::Success, HealthVerdict::Failure(down())] {
            assert_eq!(
                settle(config(1), &closed_again, earlier, outcome, now),
                closed_again
            );
        }
        let counting = at(1, Phase::Closed { failures: 2 });
        assert_eq!(
            settle(config(3), &counting, earlier, HealthVerdict::Success, now),
            counting,
            "a late success from before the opening resets nothing"
        );
    }

    #[test]
    fn a_late_outcome_of_a_call_let_through_while_closed_does_not_move_open() {
        let now = Instant::now();
        let open = at(
            1,
            Phase::Open {
                until: now + COOLDOWN,
                last_failure: down(),
            },
        );
        for outcome in [
            HealthVerdict::Success,
            HealthVerdict::Failure(down()),
            HealthVerdict::Ignored,
        ] {
            assert_eq!(settle(config(1), &open, ORDINARY, outcome, now), open);
        }
    }

    #[test]
    fn in_half_open_only_the_probes_outcome_counts() {
        let now = Instant::now();
        let busy = at(
            1,
            Phase::HalfOpen {
                probe_in_flight: true,
                last_failure: down(),
            },
        );
        assert_eq!(
            settle(config(1), &busy, ORDINARY, HealthVerdict::Success, now),
            busy
        );
        assert_eq!(
            settle(
                config(1),
                &busy,
                Admission::Probe,
                HealthVerdict::Success,
                now
            ),
            at(1, Phase::Closed { failures: 0 })
        );
        assert_eq!(
            settle(
                config(1),
                &busy,
                Admission::Probe,
                HealthVerdict::Failure(down()),
                now
            ),
            at(
                2,
                Phase::Open {
                    until: now + COOLDOWN,
                    last_failure: down()
                }
            )
        );
        assert_eq!(
            settle(
                config(1),
                &busy,
                Admission::Probe,
                HealthVerdict::Ignored,
                now
            ),
            at(
                1,
                Phase::HalfOpen {
                    probe_in_flight: false,
                    last_failure: down()
                }
            )
        );
    }

    // ── the wrapper ──────────────────────────────────────────────────

    /// Streams the same items on every call, or fails to open with the same error, and counts
    /// the calls.
    struct Scripted {
        id: ProviderId,
        capabilities: ProviderProfile,
        script: Script,
        calls: AtomicU32,
    }

    enum Script {
        Items(fn() -> Vec<Result<ChatEvent, ProviderError>>),
        FailToOpen(fn() -> ProviderError),
    }

    #[async_trait]
    impl LlmProvider for Scripted {
        fn id(&self) -> &ProviderId {
            &self.id
        }
        fn capabilities(&self) -> &ProviderProfile {
            &self.capabilities
        }
        async fn stream(
            self: Arc<Self>,
            _req: ChatRequest,
            _model: &str,
            _ctx: Caller,
        ) -> Result<LlmEventStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.script {
                Script::Items(items) => Ok(Box::pin(futures::stream::iter(items()))),
                Script::FailToOpen(e) => Err(e()),
            }
        }
    }

    fn scripted(script: Script) -> Arc<Scripted> {
        Arc::new(Scripted {
            id: ProviderId::new("p"),
            capabilities: ProviderProfile::text_only_baseline(tars_types::Pricing::default()),
            script,
            calls: AtomicU32::new(0),
        })
    }

    fn breaker(inner: Arc<dyn LlmProvider>, threshold: u32) -> Arc<CircuitBreaker> {
        CircuitBreaker::closed(inner, config(threshold))
    }

    fn delta_then_overloaded() -> Vec<Result<ChatEvent, ProviderError>> {
        vec![
            Ok(ChatEvent::Delta { text: "x".into() }),
            Err(ProviderError::ModelOverloaded),
        ]
    }

    fn finished() -> Vec<Result<ChatEvent, ProviderError>> {
        vec![
            Ok(ChatEvent::Delta { text: "x".into() }),
            Ok(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Some(Usage::default()),
            }),
        ]
    }

    fn truncated() -> Vec<Result<ChatEvent, ProviderError>> {
        vec![Ok(ChatEvent::Delta { text: "par".into() })]
    }

    fn ctx() -> Caller {
        Caller::test_default()
    }

    async fn drain(stream: LlmEventStream) {
        let mut s = stream;
        while s.next().await.is_some() {}
    }

    fn open_now(cb: &CircuitBreaker) {
        let mut state = cb.lock_state();
        *state = state.opened(Instant::now(), down());
    }

    fn half_open_busy() -> Phase {
        Phase::HalfOpen {
            probe_in_flight: true,
            last_failure: down(),
        }
    }

    #[tokio::test]
    async fn an_open_breaker_rejects_without_calling_the_provider() {
        let inner = scripted(Script::FailToOpen(|| ProviderError::ModelOverloaded));
        let cb = breaker(inner.clone(), 2);
        for _ in 0..2 {
            let r = cb.clone().stream(ChatRequest::user("x"), "m", ctx()).await;
            assert!(matches!(r, Err(ProviderError::ModelOverloaded)));
        }
        let r = cb.clone().stream(ChatRequest::user("x"), "m", ctx()).await;
        match r {
            Err(ProviderError::Unavailable { last_failure, .. }) => assert_eq!(
                last_failure,
                LastFailure::of(&ProviderError::ModelOverloaded),
                "the failure that took it out"
            ),
            Err(other) => panic!("expected Unavailable, got {other:?}"),
            Ok(_) => panic!("expected Unavailable, got a stream"),
        }
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unparseable_responses_open_the_breaker() {
        let cb = breaker(
            scripted(Script::FailToOpen(|| {
                let html = "<html>502 Bad Gateway</html>";
                ProviderError::Parse {
                    at: "a test response",
                    line: None,
                    problem: Box::new(ParseProblem::Json(
                        serde_json::from_str::<serde_json::Value>(html).unwrap_err(),
                    )),
                    raw: Some(html.into()),
                }
            })),
            2,
        );
        for _ in 0..2 {
            let r = cb.clone().stream(ChatRequest::user("x"), "m", ctx()).await;
            assert!(matches!(r, Err(ProviderError::Parse { .. })));
        }
        assert!(matches!(cb.current().phase, Phase::Open { .. }));
    }

    #[tokio::test]
    async fn a_mid_stream_failure_counts_through_stream() {
        let cb = breaker(scripted(Script::Items(delta_then_overloaded)), 1);
        let s = cb
            .clone()
            .stream(ChatRequest::user("x"), "m", ctx())
            .await
            .unwrap();
        drain(s).await;
        assert!(matches!(cb.current().phase, Phase::Open { .. }));
    }

    #[tokio::test]
    async fn a_mid_stream_failure_counts_through_complete() {
        let cb = breaker(scripted(Script::Items(delta_then_overloaded)), 1);
        let r = cb
            .clone()
            .complete(ChatRequest::user("x"), "m", ctx())
            .await;
        assert!(matches!(r, Err(ProviderError::ModelOverloaded)));
        assert!(matches!(cb.current().phase, Phase::Open { .. }));
    }

    #[tokio::test]
    async fn a_callers_error_does_not_open_the_breaker() {
        let cb = breaker(
            scripted(Script::FailToOpen(|| {
                ProviderError::InvalidRequest(RequestRefusal::EmptyMessages)
            })),
            1,
        );
        let r = cb.clone().stream(ChatRequest::user("x"), "m", ctx()).await;
        assert!(matches!(r, Err(ProviderError::InvalidRequest(_))));
        assert_eq!(cb.current(), BreakerState::CLOSED);
    }

    #[tokio::test]
    async fn a_stream_dropped_before_its_end_leaves_no_verdict() {
        let cb = breaker(scripted(Script::Items(delta_then_overloaded)), 1);
        let mut s = cb
            .clone()
            .stream(ChatRequest::user("x"), "m", ctx())
            .await
            .unwrap();
        assert!(matches!(s.next().await, Some(Ok(ChatEvent::Delta { .. }))));
        drop(s);
        assert_eq!(cb.current(), BreakerState::CLOSED);
    }

    #[tokio::test]
    async fn a_finished_probe_stream_closes_the_breaker() {
        let cb = breaker(scripted(Script::Items(finished)), 1);
        open_now(&cb);
        let s = cb
            .clone()
            .stream(ChatRequest::user("x"), "m", ctx())
            .await
            .unwrap();
        assert_eq!(cb.current().phase, half_open_busy());
        drain(s).await;
        assert_eq!(cb.current().phase, Phase::Closed { failures: 0 });
    }

    #[tokio::test]
    async fn a_probe_stream_that_ends_without_finished_frees_the_slot_and_stays_half_open() {
        let cb = breaker(scripted(Script::Items(truncated)), 1);
        open_now(&cb);
        let s = cb
            .clone()
            .stream(ChatRequest::user("x"), "m", ctx())
            .await
            .unwrap();
        drain(s).await;
        assert_eq!(
            cb.current().phase,
            Phase::HalfOpen {
                probe_in_flight: false,
                last_failure: down()
            }
        );
    }

    #[tokio::test]
    async fn a_probe_complete_without_a_stop_reason_stays_half_open() {
        let cb = breaker(scripted(Script::Items(truncated)), 1);
        open_now(&cb);
        let r = cb
            .clone()
            .complete(ChatRequest::user("x"), "m", ctx())
            .await;
        assert!(matches!(r, Err(ProviderError::Unfinished(_))), "{r:?}");
        assert_eq!(
            cb.current().phase,
            Phase::HalfOpen {
                probe_in_flight: false,
                last_failure: down()
            }
        );
    }

    #[tokio::test]
    async fn a_probe_stream_dropped_unread_frees_the_probe_slot() {
        let cb = breaker(scripted(Script::Items(finished)), 1);
        open_now(&cb);
        let s = cb
            .clone()
            .stream(ChatRequest::user("x"), "m", ctx())
            .await
            .unwrap();
        drop(s);
        assert_eq!(
            cb.current().phase,
            Phase::HalfOpen {
                probe_in_flight: false,
                last_failure: down()
            }
        );
    }

    /// The first call streams from a channel the test holds; every later call finishes at once.
    struct HeldThenFinished {
        id: ProviderId,
        capabilities: ProviderProfile,
        held: Mutex<Option<mpsc::UnboundedReceiver<Result<ChatEvent, ProviderError>>>>,
    }

    #[async_trait]
    impl LlmProvider for HeldThenFinished {
        fn id(&self) -> &ProviderId {
            &self.id
        }
        fn capabilities(&self) -> &ProviderProfile {
            &self.capabilities
        }
        async fn stream(
            self: Arc<Self>,
            _req: ChatRequest,
            _model: &str,
            _ctx: Caller,
        ) -> Result<LlmEventStream, ProviderError> {
            let held = self.held.lock().unwrap().take();
            match held {
                Some(mut rx) => Ok(Box::pin(async_stream::stream! {
                    while let Some(item) = rx.recv().await {
                        yield item;
                    }
                })),
                None => Ok(Box::pin(futures::stream::iter(finished()))),
            }
        }
    }

    #[tokio::test]
    async fn a_call_from_before_an_opening_does_not_move_the_breaker_after_it_closes_again() {
        let (tx, rx) = mpsc::unbounded_channel();
        let cb = breaker(
            Arc::new(HeldThenFinished {
                id: ProviderId::new("p"),
                capabilities: ProviderProfile::text_only_baseline(tars_types::Pricing::default()),
                held: Mutex::new(Some(rx)),
            }),
            1,
        );
        let slow = cb
            .clone()
            .stream(ChatRequest::user("x"), "m", ctx())
            .await
            .unwrap();
        open_now(&cb);
        drain(
            cb.clone()
                .stream(ChatRequest::user("x"), "m", ctx())
                .await
                .unwrap(),
        )
        .await;
        let closed_again = cb.current();
        assert_eq!(closed_again.phase, Phase::Closed { failures: 0 });

        tx.send(Err(ProviderError::ModelOverloaded)).unwrap();
        drop(tx);
        drain(slow).await;
        assert_eq!(cb.current(), closed_again);
    }

    #[test]
    fn an_unavailable_provider_is_transient_and_says_until_when() {
        use tars_types::ErrorClass;
        let until = Instant::now() + Duration::from_secs(30);
        let e = ProviderError::Unavailable {
            until,
            last_failure: down(),
        };
        assert_eq!(e.class(), ErrorClass::Transient);
        let ra = e.unavailable_for().unwrap();
        assert!(ra <= Duration::from_secs(31));
        assert!(ra >= Duration::from_secs(28));
    }

    struct ExactCounter {
        id: ProviderId,
        capabilities: ProviderProfile,
    }

    #[async_trait]
    impl LlmProvider for ExactCounter {
        fn id(&self) -> &ProviderId {
            &self.id
        }
        fn capabilities(&self) -> &ProviderProfile {
            &self.capabilities
        }
        async fn stream(
            self: Arc<Self>,
            _req: ChatRequest,
            _model: &str,
            _ctx: Caller,
        ) -> Result<LlmEventStream, ProviderError> {
            Ok(Box::pin(futures::stream::iter(finished())))
        }
        async fn count_tokens(
            &self,
            _req: &ChatRequest,
            _fast: bool,
        ) -> Result<u64, ProviderError> {
            Ok(42)
        }
    }

    #[tokio::test]
    async fn a_breaker_counts_tokens_with_the_inner_providers_tokenizer() {
        let cb = breaker(
            Arc::new(ExactCounter {
                id: ProviderId::new("p"),
                capabilities: ProviderProfile::text_only_baseline(tars_types::Pricing::default()),
            }),
            1,
        );
        assert_eq!(
            cb.count_tokens(&ChatRequest::user("x"), false)
                .await
                .unwrap(),
            42
        );
    }

    #[test]
    fn a_breaker_keeps_the_inner_providers_batch_api() {
        let http = crate::provider::http_base::HttpClient::default_arc().unwrap();
        let inner = crate::OpenAiProviderBuilder::new(
            ProviderId::new("openai"),
            crate::provider::auth::Auth::None,
        )
        .build(http, crate::provider::auth::basic());
        assert!(breaker(inner, 1).as_batch_submitter().is_some());
    }

    #[test]
    fn wrap_preserves_id_and_capabilities() {
        let inner = scripted(Script::Items(finished));
        let marker = inner.capabilities().max_context_tokens;
        let cb = CircuitBreaker::wrap(inner, CircuitBreakerConfig::default());
        assert_eq!(cb.id().as_ref(), "p");
        assert_eq!(cb.capabilities().max_context_tokens, marker);
    }

    #[test]
    fn a_breaker_section_sets_only_the_keys_it_names() {
        let cfg: CircuitBreakerConfig =
            toml::from_str("cooldown_secs = 60\n").expect("a partial [llm.circuit_breaker] reads");
        assert_eq!(
            cfg,
            CircuitBreakerConfig::new(DEFAULT_FAILURE_THRESHOLD, Duration::from_secs(60))
                .expect("a minute is a cooldown")
        );
    }

    #[test]
    fn a_breaker_section_is_checked_as_new_checks_it() {
        let day = MAX_COOLDOWN.as_secs();
        assert!(
            toml::from_str::<CircuitBreakerConfig>(&format!("cooldown_secs = {day}\n")).is_ok()
        );
        let err = toml::from_str::<CircuitBreakerConfig>(&format!("cooldown_secs = {}\n", day + 1))
            .unwrap_err();
        assert!(err.to_string().contains("exceeds the maximum"), "{err}");
        assert!(toml::from_str::<CircuitBreakerConfig>("failure_threshold = 0\n").is_err());
        assert!(toml::from_str::<CircuitBreakerConfig>("cooldown = 5\n").is_err());
    }

    #[test]
    fn the_default_breaker_writes_and_reads_back_unchanged() {
        let written =
            toml::to_string(&CircuitBreakerConfig::default()).expect("the default writes");
        assert_eq!(
            toml::from_str::<CircuitBreakerConfig>(&written).expect("and reads back"),
            CircuitBreakerConfig::default()
        );
    }
}
