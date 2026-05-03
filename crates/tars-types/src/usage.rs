//! Token + cost accounting.

use serde::{Deserialize, Serialize};

/// Token usage for a single request. All fields are cumulative for the
/// entire response (input + all output, including tool calls and
/// thinking tokens).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens billed at full rate.
    pub input_tokens: u64,
    /// Output tokens (completion + tool args + thinking, depending on provider).
    pub output_tokens: u64,
    /// Tokens served from prefix cache. **Discount accounting only**:
    /// these are *also* counted in `input_tokens` per Anthropic /
    /// OpenAI's conventions, so subtracting from `input_tokens` would
    /// double-count. The `cached_input_tokens` figure represents the
    /// portion that was billed at the discounted rate.
    pub cached_input_tokens: u64,
    /// Tokens spent on cache *creation* (Anthropic only). These are
    /// billed at the cache-creation rate (≥ standard input rate).
    pub cache_creation_tokens: u64,
    /// Provider-side internal "thinking" tokens, when the provider
    /// distinguishes them from output. Not all providers expose this.
    pub thinking_tokens: u64,
}

impl Usage {
    /// Sum two usage records — useful for accumulating across retries
    /// or chained calls. Saturating to prevent overflow surprises.
    pub fn merge(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_add(other.cached_input_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_add(other.cache_creation_tokens),
            thinking_tokens: self.thinking_tokens.saturating_add(other.thinking_tokens),
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Cost in USD as a fixed-point value. `f64` would suffice for
/// display, but we use a wrapper so accidental arithmetic with raw
/// floats is rejected at compile time.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CostUsd(pub f64);

impl CostUsd {
    pub fn zero() -> Self {
        Self(0.0)
    }

    pub fn as_f64(&self) -> f64 {
        self.0
    }
}

impl std::ops::Add for CostUsd {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl std::iter::Sum for CostUsd {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        Self(iter.map(|c| c.0).sum())
    }
}

/// Per-model pricing. All units are USD per 1M tokens. We deliberately
/// keep this in the type layer so the provider doesn't need to know
/// the table; the registry / config layer computes prices.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Pricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    /// Discount rate for cached input tokens — typical 25-50% of standard.
    pub cached_input_per_million: f64,
    /// Cache *creation* surcharge (Anthropic) — typical 125% of standard.
    pub cache_creation_per_million: f64,
}

impl Pricing {
    pub fn cost_for(&self, usage: &Usage) -> CostUsd {
        // `cached_input_tokens` is *included* in `input_tokens` per
        // provider convention, so subtract before applying the full
        // input rate. Same for cache_creation_tokens.
        let billable_input = usage
            .input_tokens
            .saturating_sub(usage.cached_input_tokens)
            .saturating_sub(usage.cache_creation_tokens);
        let total = (billable_input as f64) * self.input_per_million / 1_000_000.0
            + (usage.output_tokens as f64) * self.output_per_million / 1_000_000.0
            + (usage.cached_input_tokens as f64) * self.cached_input_per_million
                / 1_000_000.0
            + (usage.cache_creation_tokens as f64) * self.cache_creation_per_million
                / 1_000_000.0;
        CostUsd(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_saturates() {
        let a = Usage { input_tokens: u64::MAX, ..Default::default() };
        let b = Usage { input_tokens: 5, ..Default::default() };
        assert_eq!(a.merge(b).input_tokens, u64::MAX);
    }

    #[test]
    fn pricing_subtracts_cached_from_billable_input() {
        let p = Pricing {
            input_per_million: 10.0,
            output_per_million: 30.0,
            cached_input_per_million: 1.0,
            cache_creation_per_million: 12.5,
        };
        // 1000 input total, 200 of which were cached.
        let u = Usage {
            input_tokens: 1000,
            output_tokens: 100,
            cached_input_tokens: 200,
            ..Default::default()
        };
        // billable = 800 -> 800 * 10 / 1e6 = 0.008
        // output   = 100 -> 100 * 30 / 1e6 = 0.003
        // cached   = 200 -> 200 * 1  / 1e6 = 0.0002
        // total    = 0.0112
        let c = p.cost_for(&u).0;
        assert!((c - 0.0112).abs() < 1e-9, "got {c}");
    }
}
