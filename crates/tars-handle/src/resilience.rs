//! Bridge the `[resilience]` config section
//! ([`tars_config::ResilienceConfig`]) onto the pipeline's retry +
//! circuit-breaker knobs ([`tars_pipeline::RetryConfig`] /
//! [`tars_pipeline::CircuitBreakerConfig`]).
//!
//! This lives in `tars-handle`, the composition layer that already maps
//! config → [`tars_pipeline::ChainOpts`], rather than in either leaf crate:
//! `tars-config` is schema-only and must not know the pipeline types, and
//! `tars-pipeline` is the low-level middleware framework that must not depend
//! on the config crate (the dependency runs config → pipeline, never the
//! reverse). So the handle owns the bridge — free functions here (the orphan
//! rule rules out a `From` impl on two foreign types).
//!
//! Each tuning struct is a **sparse overlay**: a `None` field falls back to
//! the tars-pipeline `Default::default()` value, keeping the default constants
//! single-sourced in [`RetryConfig::default`] / [`CircuitBreakerConfig::default`].
//! The `*_secs → Duration` conversion is panic-safe (a non-finite / negative /
//! overflowing value — which [`ResilienceConfig::validate`] already rejects at
//! load time — falls back to the default rather than panicking in
//! `Duration::try_from_secs_f64`).

use std::time::Duration;

use tars_config::{BreakerTuning, ResilienceConfig, RetryTuning};
use tars_pipeline::{CircuitBreakerConfig, RetryConfig};

/// Map a `[resilience]` section onto the two [`tars_pipeline::ChainOpts`]
/// knobs `retry` / `circuit_breaker`.
///
/// - `retry: None` ⇒ `None` ⇒ `default_chain` uses [`RetryConfig::default`].
/// - `circuit_breaker: None` ⇒ `None` ⇒ no breaker (today's behaviour).
///
/// So an empty (or absent) `[resilience]` leaves the pipeline exactly as it is
/// today; a populated one flows straight into
/// [`tars_pipeline::LlmService::default_chain`].
pub fn resilience_configs(
    cfg: &ResilienceConfig,
) -> (Option<RetryConfig>, Option<CircuitBreakerConfig>) {
    (
        cfg.retry.as_ref().map(retry_config),
        cfg.circuit_breaker.as_ref().map(breaker_config),
    )
}

/// Seconds → `Duration`, falling back to `default` for a value
/// `Duration::try_from_secs_f64` rejects (negative / NaN / overflow).
fn secs_or(v: Option<f64>, default: Duration) -> Duration {
    match v {
        Some(s) => Duration::try_from_secs_f64(s).unwrap_or(default),
        None => default,
    }
}

fn retry_config(t: &RetryTuning) -> RetryConfig {
    let d = RetryConfig::default();
    RetryConfig {
        max_attempts: t.max_attempts.unwrap_or(d.max_attempts),
        initial_backoff: secs_or(t.initial_backoff_secs, d.initial_backoff),
        max_backoff: secs_or(t.max_backoff_secs, d.max_backoff),
        multiplier: t.multiplier.unwrap_or(d.multiplier),
        respect_retry_after: t.respect_retry_after.unwrap_or(d.respect_retry_after),
        max_attempts_maybe_retriable: t
            .max_attempts_maybe_retriable
            .unwrap_or(d.max_attempts_maybe_retriable),
        max_wait: secs_or(t.max_wait_secs, d.max_wait),
        jitter: secs_or(t.jitter_secs, d.jitter),
    }
}

fn breaker_config(t: &BreakerTuning) -> CircuitBreakerConfig {
    let d = CircuitBreakerConfig::default();
    CircuitBreakerConfig {
        failure_threshold: t.failure_threshold.unwrap_or(d.failure_threshold),
        cooldown: secs_or(t.cooldown_secs, d.cooldown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_none_maps_to_none_none() {
        let (retry, breaker) = resilience_configs(&ResilienceConfig::default());
        assert!(retry.is_none(), "absent retry ⇒ None ⇒ default_chain default");
        assert!(breaker.is_none(), "absent breaker ⇒ None ⇒ no breaker");
    }

    #[test]
    fn absent_retry_fields_fall_back_to_defaults() {
        let cfg = ResilienceConfig {
            retry: Some(RetryTuning {
                max_attempts: Some(6),
                ..Default::default()
            }),
            circuit_breaker: None,
        };
        let (retry, breaker) = resilience_configs(&cfg);
        let r = retry.expect("retry present");
        let d = RetryConfig::default();
        assert_eq!(r.max_attempts, 6);
        assert_eq!(r.initial_backoff, d.initial_backoff);
        assert_eq!(r.max_backoff, d.max_backoff);
        assert_eq!(r.multiplier, d.multiplier);
        assert_eq!(r.max_wait, d.max_wait);
        assert_eq!(r.jitter, d.jitter);
        assert!(breaker.is_none());
    }

    #[test]
    fn arc_policy_maps_to_expected_configs() {
        let cfg = ResilienceConfig {
            retry: Some(RetryTuning {
                max_attempts: Some(6),
                initial_backoff_secs: Some(1.0),
                max_backoff_secs: Some(30.0),
                multiplier: Some(2.0),
                respect_retry_after: Some(true),
                max_attempts_maybe_retriable: Some(1),
                max_wait_secs: Some(30.0),
                jitter_secs: Some(0.4),
            }),
            circuit_breaker: Some(BreakerTuning {
                failure_threshold: Some(4),
                cooldown_secs: Some(30.0),
            }),
        };
        let (retry, breaker) = resilience_configs(&cfg);
        let r = retry.expect("retry");
        assert_eq!(r.max_attempts, 6);
        assert_eq!(r.initial_backoff, Duration::from_secs(1));
        assert_eq!(r.max_backoff, Duration::from_secs(30));
        assert_eq!(r.multiplier, 2.0);
        assert!(r.respect_retry_after);
        assert_eq!(r.max_attempts_maybe_retriable, 1);
        assert_eq!(r.max_wait, Duration::from_secs(30));
        assert_eq!(r.jitter, Duration::from_millis(400));
        let b = breaker.expect("breaker");
        assert_eq!(b.failure_threshold, 4);
        assert_eq!(b.cooldown, Duration::from_secs(30));
    }

    #[test]
    fn empty_breaker_table_enables_breaker_with_defaults() {
        let cfg = ResilienceConfig {
            retry: None,
            circuit_breaker: Some(BreakerTuning::default()),
        };
        let (_retry, breaker) = resilience_configs(&cfg);
        let b = breaker.expect("present table ⇒ breaker on");
        let d = CircuitBreakerConfig::default();
        assert_eq!(b.failure_threshold, d.failure_threshold);
        assert_eq!(b.cooldown, d.cooldown);
    }

    #[test]
    fn non_finite_secs_falls_back_to_default_without_panic() {
        // validate() rejects this at load, but the conversion must be
        // panic-safe if a caller builds the struct directly.
        let cfg = ResilienceConfig {
            retry: Some(RetryTuning {
                initial_backoff_secs: Some(f64::NAN),
                jitter_secs: Some(-5.0),
                ..Default::default()
            }),
            circuit_breaker: None,
        };
        let (retry, _) = resilience_configs(&cfg);
        let r = retry.unwrap();
        let d = RetryConfig::default();
        assert_eq!(r.initial_backoff, d.initial_backoff, "NaN ⇒ default");
        assert_eq!(r.jitter, d.jitter, "negative ⇒ default");
    }
}
