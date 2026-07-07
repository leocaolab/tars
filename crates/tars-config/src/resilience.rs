//! `[resilience]` config section — LLM transport retry + circuit breaker
//! tuning, so consumers stop hand-copying the constants.
//!
//! Every real consumer (arc's `arc_resilience()`, concer's
//! `concer_resilience`) used to re-type the same `RetryConfig` /
//! `CircuitBreakerConfig` literals at pipeline-build time — the only way to
//! deviate from tars's defaults was to hardcode a full config in Rust. This
//! section moves that policy into `$TARS_HOME/config.toml` so the
//! [`Tars`](../../tars_handle/struct.Tars.html) handle reads it and feeds
//! every handle-built pipeline.
//!
//! **Default = tars's CURRENT behaviour.** The whole section is optional and
//! both sub-tables are `Option`:
//!
//! - `retry` absent ⇒ `None` ⇒ `default_chain` uses
//!   [`RetryConfig::default`](../../tars_pipeline/struct.RetryConfig.html)
//!   (3 attempts, exp backoff, 30s cap) — unchanged.
//! - `circuit_breaker` absent ⇒ `None` ⇒ **no breaker** — unchanged.
//!
//! So an existing config with no `[resilience]` table produces exactly the
//! pipeline it does today.
//!
//! ## Overlay semantics
//!
//! Both tuning structs are a **sparse overlay**: every field is `Option`, and
//! an omitted field falls back to the corresponding tars-pipeline default at
//! conversion time (the conversion lives in `tars-pipeline`, which owns the
//! target types — this crate can't depend on it without a cycle). So
//! `[resilience.retry] max_attempts = 6` overrides only `max_attempts` and
//! leaves the rest at tars defaults. **Presence** of the `[resilience.circuit_breaker]`
//! table (even empty) enables the breaker with default thresholds; absence
//! leaves it off.
//!
//! Durations are expressed as **seconds** (`f64`, so sub-second values like a
//! 0.4s jitter are representable).
//!
//! ```toml
//! [resilience.retry]
//! max_attempts = 6
//! initial_backoff_secs = 1.0
//! max_backoff_secs = 30.0
//! multiplier = 2.0
//! respect_retry_after = true
//! max_attempts_maybe_retriable = 1
//! max_wait_secs = 30.0
//! jitter_secs = 0.4
//!
//! [resilience.circuit_breaker]
//! failure_threshold = 4
//! cooldown_secs = 30.0
//! ```

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

/// The `[resilience]` table. Both sub-sections optional; absence = tars's
/// current behaviour (default retry, no breaker).
///
/// The conversion into tars-pipeline's `RetryConfig` /
/// `CircuitBreakerConfig` lives in `tars-pipeline` (it owns those types and
/// already depends on this crate; the reverse dependency would be a cycle).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResilienceConfig {
    /// Retry policy overlay. `None` ⇒ tars-pipeline `RetryConfig::default()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryTuning>,

    /// Circuit-breaker overlay. `None` ⇒ no breaker (today's default).
    /// Present (even empty) ⇒ breaker enabled with defaults filled in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_breaker: Option<BreakerTuning>,
}

/// Sparse overlay over tars-pipeline's `RetryConfig`. Every field is
/// `Option`; omitted fields fall back to `RetryConfig::default()` at
/// conversion time (keeping the default constants single-sourced in
/// tars-pipeline). Durations are seconds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryTuning {
    /// Total attempts including the first try. `1` disables retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// First backoff, seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_backoff_secs: Option<f64>,
    /// Backoff ceiling, seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_backoff_secs: Option<f64>,
    /// Exponential growth factor per attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiplier: Option<f64>,
    /// Prefer a provider `Retry-After` over our computed backoff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub respect_retry_after: Option<bool>,
    /// Cap for `MaybeRetriable` errors (parse/subprocess smells).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts_maybe_retriable: Option<u32>,
    /// Upper bound on any single wait (Retry-After or computed), seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_wait_secs: Option<f64>,
    /// Max random jitter added to a *computed* backoff, seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jitter_secs: Option<f64>,
}

/// Sparse overlay over tars-pipeline's `CircuitBreakerConfig`. Presence of
/// the table enables the breaker; omitted fields fall back to
/// `CircuitBreakerConfig::default()`. Durations are seconds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BreakerTuning {
    /// Open after this many consecutive open-time failures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_threshold: Option<u32>,
    /// How long an Open breaker stays Open before HalfOpen, seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_secs: Option<f64>,
}

impl ResilienceConfig {
    /// The historical LLM-transport resilience policy every real consumer
    /// (arc, concer) used to hand-copy as Rust literals: **6 attempts**
    /// (1 initial + 5 retries), 1s→30s exponential backoff (×2, capped 30s)
    /// with up to 400ms jitter, honouring a provider `Retry-After` capped at
    /// `max_wait` (30s), `MaybeRetriable` errors get a single attempt; a
    /// **circuit breaker** that opens after 4 consecutive failures with a 30s
    /// cooldown.
    ///
    /// These NUMBERS live here, ONCE. A consumer that wants this policy (rather
    /// than tars-pipeline's leaner 3-attempt / no-breaker default) reads
    /// `config.resilience` and falls back to this when the section is absent —
    /// see [`ResilienceConfig::or_llm_default`]. Every field is populated
    /// (`Some`), so the conversion in `tars_handle::resilience::resilience_configs`
    /// yields exactly this policy with no reliance on tars-pipeline defaults.
    pub fn llm_default() -> Self {
        ResilienceConfig {
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
        }
    }

    /// True when no `[resilience]` section was present at all — both sub-tables
    /// absent. This is exactly `ResilienceConfig::default()` and reproduces
    /// tars-pipeline's built-in behaviour (default retry, no breaker).
    pub fn is_empty(&self) -> bool {
        self.retry.is_none() && self.circuit_breaker.is_none()
    }

    /// The effective policy for a consumer that wants the LLM default when the
    /// operator wrote no `[resilience]` section: returns [`llm_default`] when
    /// [`is_empty`], otherwise the config as written (so a present section —
    /// even a partial one — always wins, preserving the sparse-overlay
    /// override semantics; only the *total absence* of the section triggers the
    /// LLM default).
    ///
    /// [`llm_default`]: ResilienceConfig::llm_default
    /// [`is_empty`]: ResilienceConfig::is_empty
    pub fn or_llm_default(self) -> Self {
        if self.is_empty() { Self::llm_default() } else { self }
    }

    /// Validate the numeric ranges. Counts must be `>= 1`; every `*_secs`
    /// must be finite and non-negative; `multiplier` finite and non-negative.
    /// The conversion in tars-pipeline is panic-safe regardless, but a bad
    /// value is an operator mistake worth surfacing at load time rather than
    /// silently clamping.
    pub fn validate(&self, errs: &mut Vec<ValidationError>) {
        if let Some(r) = &self.retry {
            check_count(
                "resilience.retry.max_attempts",
                r.max_attempts,
                errs,
            );
            check_count(
                "resilience.retry.max_attempts_maybe_retriable",
                r.max_attempts_maybe_retriable,
                errs,
            );
            check_secs(
                "resilience.retry.initial_backoff_secs",
                r.initial_backoff_secs,
                errs,
            );
            check_secs("resilience.retry.max_backoff_secs", r.max_backoff_secs, errs);
            check_secs("resilience.retry.max_wait_secs", r.max_wait_secs, errs);
            check_secs("resilience.retry.jitter_secs", r.jitter_secs, errs);
            if let Some(m) = r.multiplier {
                if !m.is_finite() || m < 0.0 {
                    errs.push(ValidationError::new(
                        "resilience.retry.multiplier",
                        format!("must be a finite, non-negative number (got {m})"),
                    ));
                }
            }
        }
        if let Some(b) = &self.circuit_breaker {
            check_count(
                "resilience.circuit_breaker.failure_threshold",
                b.failure_threshold,
                errs,
            );
            check_secs(
                "resilience.circuit_breaker.cooldown_secs",
                b.cooldown_secs,
                errs,
            );
        }
    }
}

/// An attempt-count knob must be `>= 1` when set: `0` would mean "never even
/// try once", which is never what an operator wants from a retry config.
fn check_count(key: &str, v: Option<u32>, errs: &mut Vec<ValidationError>) {
    if let Some(n) = v {
        if n < 1 {
            errs.push(ValidationError::new(
                key,
                format!("must be >= 1 (got {n})"),
            ));
        }
    }
}

/// A duration-in-seconds knob must be finite and non-negative when set.
fn check_secs(key: &str, v: Option<f64>, errs: &mut Vec<ValidationError>) {
    if let Some(s) = v {
        if !s.is_finite() || s < 0.0 {
            errs.push(ValidationError::new(
                key,
                format!("must be a finite, non-negative number of seconds (got {s})"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConfigManager;

    #[test]
    fn absent_section_is_default_none_none() {
        let cfg = ConfigManager::load_from_str("[providers]\n").unwrap();
        assert_eq!(cfg.resilience, ResilienceConfig::default());
        assert!(cfg.resilience.retry.is_none());
        assert!(cfg.resilience.circuit_breaker.is_none());
    }

    #[test]
    fn full_section_parses_arc_policy() {
        let toml = r#"
            [resilience.retry]
            max_attempts = 6
            initial_backoff_secs = 1.0
            max_backoff_secs = 30.0
            multiplier = 2.0
            respect_retry_after = true
            max_attempts_maybe_retriable = 1
            max_wait_secs = 30.0
            jitter_secs = 0.4

            [resilience.circuit_breaker]
            failure_threshold = 4
            cooldown_secs = 30.0
        "#;
        let cfg = ConfigManager::load_from_str(toml).unwrap();
        let r = cfg.resilience.retry.unwrap();
        assert_eq!(r.max_attempts, Some(6));
        assert_eq!(r.initial_backoff_secs, Some(1.0));
        assert_eq!(r.max_attempts_maybe_retriable, Some(1));
        assert_eq!(r.jitter_secs, Some(0.4));
        let b = cfg.resilience.circuit_breaker.unwrap();
        assert_eq!(b.failure_threshold, Some(4));
        assert_eq!(b.cooldown_secs, Some(30.0));
    }

    #[test]
    fn partial_retry_table_leaves_other_fields_none() {
        let cfg = ConfigManager::load_from_str(
            "[resilience.retry]\nmax_attempts = 6\n",
        )
        .unwrap();
        let r = cfg.resilience.retry.unwrap();
        assert_eq!(r.max_attempts, Some(6));
        assert_eq!(r.multiplier, None, "unset field stays None (filled at convert)");
        assert!(cfg.resilience.circuit_breaker.is_none());
    }

    #[test]
    fn empty_breaker_table_enables_breaker_with_defaults() {
        // Presence of the table (even with no fields) = Some = breaker on.
        let cfg = ConfigManager::load_from_str("[resilience.circuit_breaker]\n").unwrap();
        let b = cfg.resilience.circuit_breaker.expect("present ⇒ Some");
        assert_eq!(b.failure_threshold, None, "defaults filled at convert time");
    }

    #[test]
    fn unknown_field_rejected_by_deny_unknown_fields() {
        let err = ConfigManager::load_from_str(
            "[resilience.retry]\nmax_attemps = 6\n", // typo
        );
        assert!(err.is_err(), "typo'd key must be caught by deny_unknown_fields");
    }

    #[test]
    fn zero_max_attempts_fails_validation() {
        let err = ConfigManager::load_from_str(
            "[resilience.retry]\nmax_attempts = 0\n",
        )
        .unwrap_err();
        match err {
            crate::ConfigError::ValidationFailed { errors } => {
                assert!(
                    errors
                        .iter()
                        .any(|e| e.key == "resilience.retry.max_attempts")
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn llm_default_carries_the_six_breaker_policy() {
        // The single source of the historical arc/concer numbers. A drift here
        // is the deliberate way to change the shared policy — and it changes
        // both consumers at once, which is the point.
        let r = ResilienceConfig::llm_default();
        let retry = r.retry.clone().expect("llm_default has retry");
        assert_eq!(retry.max_attempts, Some(6));
        assert_eq!(retry.initial_backoff_secs, Some(1.0));
        assert_eq!(retry.max_backoff_secs, Some(30.0));
        assert_eq!(retry.multiplier, Some(2.0));
        assert_eq!(retry.respect_retry_after, Some(true));
        assert_eq!(retry.max_attempts_maybe_retriable, Some(1));
        assert_eq!(retry.max_wait_secs, Some(30.0));
        assert_eq!(retry.jitter_secs, Some(0.4));
        let breaker = r.circuit_breaker.clone().expect("llm_default has breaker");
        assert_eq!(breaker.failure_threshold, Some(4));
        assert_eq!(breaker.cooldown_secs, Some(30.0));
        // Every field populated ⇒ conversion never touches tars-pipeline defaults.
        assert!(!r.is_empty());
        // And it passes its own validation (counts >= 1, secs finite >= 0).
        let mut errs = Vec::new();
        r.validate(&mut errs);
        assert!(errs.is_empty(), "llm_default must be self-valid: {errs:?}");
    }

    #[test]
    fn or_llm_default_fills_only_when_section_absent() {
        // Absent section ⇒ default() ⇒ is_empty ⇒ fall back to the LLM policy.
        assert_eq!(
            ResilienceConfig::default().or_llm_default(),
            ResilienceConfig::llm_default(),
            "no [resilience] section ⇒ the LLM default policy"
        );
        // A present (even partial) section wins verbatim — the operator opted in.
        let partial = ResilienceConfig {
            retry: Some(RetryTuning { max_attempts: Some(2), ..Default::default() }),
            circuit_breaker: None,
        };
        assert_eq!(
            partial.clone().or_llm_default(),
            partial,
            "a present section overrides; only total absence triggers llm_default"
        );
    }

    #[test]
    fn negative_cooldown_fails_validation() {
        let err = ConfigManager::load_from_str(
            "[resilience.circuit_breaker]\ncooldown_secs = -1.0\n",
        )
        .unwrap_err();
        match err {
            crate::ConfigError::ValidationFailed { errors } => {
                assert!(
                    errors
                        .iter()
                        .any(|e| e.key == "resilience.circuit_breaker.cooldown_secs")
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
