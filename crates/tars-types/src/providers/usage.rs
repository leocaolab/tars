//! Counts the tokens a call used ([`Usage`]) and prices them ([`Pricing`], [`CostUsd`]).

use serde::{Deserialize, Serialize};

/// The tokens one request used, each field counted over the whole response. A field a
/// record does not carry reads as zero, so a partial record still reports what it has.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// The whole prompt's tokens, including `cached_input_tokens` and `cache_creation_tokens`.
    pub input_tokens: u64,
    /// Output tokens: completion and tool arguments, and thinking when the provider bundles it.
    pub output_tokens: u64,
    /// The part of `input_tokens` served from the prefix cache at the discounted rate. As in
    /// OpenAI's form it is a subset of `input_tokens`; an adapter whose wire reports them
    /// disjointly (Anthropic) adds them up when parsing, so [`Pricing::cost_for`] applies one rule.
    pub cached_input_tokens: u64,
    /// The part of `input_tokens` spent creating a cache entry (Anthropic only), billed at the
    /// cache-creation rate.
    pub cache_creation_tokens: u64,
    /// Thinking tokens, when the provider reports them apart from output.
    pub thinking_tokens: u64,
}

impl Usage {
    /// Returns the field-wise sum of two usages, saturating at `u64::MAX`.
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

    /// Returns the larger of each figure of two usage reports of one response.
    pub(crate) fn max_each(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.max(other.input_tokens),
            output_tokens: self.output_tokens.max(other.output_tokens),
            cached_input_tokens: self.cached_input_tokens.max(other.cached_input_tokens),
            cache_creation_tokens: self.cache_creation_tokens.max(other.cache_creation_tokens),
            thinking_tokens: self.thinking_tokens.max(other.thinking_tokens),
        }
    }

    /// Returns input + output + thinking tokens, saturating.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.thinking_tokens)
    }
}

/// A cost in US dollars, held as an `f64` in a wrapper so it does not mix with raw floats by
/// accident.
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
    /// Adds two costs. [`Pricing::cost_for`] only makes finite, non-negative ones, but the field
    /// is public: a non-finite operand fails a `debug_assert` and is added as is in release.
    fn add(self, rhs: Self) -> Self {
        debug_assert!(
            self.0.is_finite() && rhs.0.is_finite(),
            "CostUsd::add on non-finite operand(s): {} + {}",
            self.0,
            rhs.0
        );
        Self(self.0 + rhs.0)
    }
}

impl std::iter::Sum for CostUsd {
    /// Sums costs; a non-finite total fails a `debug_assert`, as `add` does.
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        let total: f64 = iter.map(|c| c.0).sum();
        debug_assert!(
            total.is_finite(),
            "CostUsd::sum produced a non-finite total: {total}"
        );
        Self(total)
    }
}

/// A price in US dollars per million tokens. It is finite, not negative and at most
/// [`Rate::MAX`], so any token count priced at it costs a finite, non-negative amount.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Rate(f64);

/// A number that is not a [`Rate`]: NaN, infinite, negative or above [`Rate::MAX`].
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error(
    "a rate is US dollars per million tokens from 0 to {}, got {0}",
    Rate::MAX
)]
pub struct InvalidRateError(pub f64);

impl Rate {
    pub const ZERO: Rate = Rate(0.0);
    /// One dollar per token, far above any real price; a cap keeps `u64::MAX` tokens at the
    /// highest rate a finite cost.
    pub const MAX: f64 = 1_000_000.0;

    /// Returns `usd_per_million` as a rate.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidRateError`] for NaN, an infinity, a negative number or one above
    /// [`Rate::MAX`].
    pub fn new(usd_per_million: f64) -> Result<Self, InvalidRateError> {
        if (0.0..=Self::MAX).contains(&usd_per_million) {
            Ok(Self(usd_per_million))
        } else {
            Err(InvalidRateError(usd_per_million))
        }
    }

    pub fn usd_per_million(self) -> f64 {
        self.0
    }

    /// Returns what `tokens` cost at this rate.
    fn cost_of(self, tokens: f64) -> f64 {
        tokens * self.0 / 1_000_000.0
    }
}

impl TryFrom<f64> for Rate {
    type Error = InvalidRateError;
    fn try_from(v: f64) -> Result<Self, Self::Error> {
        Self::new(v)
    }
}

impl From<Rate> for f64 {
    fn from(r: Rate) -> f64 {
        r.0
    }
}

impl std::fmt::Display for Rate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A model's prices. Config fills it in; providers carry it and do not know the price table.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Pricing {
    pub input_per_million: Rate,
    pub output_per_million: Rate,
    /// The rate of cached input tokens, typically 25-50% of the input rate.
    pub cached_input_per_million: Rate,
    /// The rate of cache-creation tokens (Anthropic), typically 125% of the input rate.
    pub cache_creation_per_million: Rate,
    /// The rate of thinking tokens a provider reports apart from output; 0 by default. Anthropic
    /// bundles thinking into `output_tokens` (0 here); Gemini reports `thoughts_token_count`
    /// separately and bills it at the output rate.
    pub thinking_per_million: Rate,
}

impl Pricing {
    /// Returns whether every rate is zero, as on a flat-rate, subscription or local plan where a
    /// per-call dollar cost means nothing; the budget middleware puts no cap on such a provider.
    pub fn is_zero(&self) -> bool {
        // A provider free on input and output can still charge for cache or thinking tokens.
        [
            self.input_per_million,
            self.output_per_million,
            self.cached_input_per_million,
            self.cache_creation_per_million,
            self.thinking_per_million,
        ]
        .iter()
        .all(|r| *r == Rate::ZERO)
    }

    /// Returns a rough pre-call cost of `req` in US dollars: its text's [`crate::estimate_tokens`]
    /// as input tokens, plus the request's `max_output_tokens` (or `default_max_output`) at the
    /// output rate. It is not an upper bound: the byte estimate misjudges multi-byte scripts, and
    /// images are not counted, their token cost being model-specific (Anthropic ~1.6k per image,
    /// OpenAI by tile count).
    pub fn estimate_chat_cost(&self, req: &crate::ChatRequest, default_max_output: u32) -> f64 {
        let input_tokens = crate::estimate_tokens(req.text_bytes());
        let max_output_tokens = req.max_output_tokens.unwrap_or(default_max_output);
        self.input_per_million.cost_of(input_tokens as f64)
            + self
                .output_per_million
                .cost_of(f64::from(max_output_tokens))
    }

    /// Returns what `usage` costs, billing the cached and cache-creation parts of `input_tokens`
    /// at their own rates and the rest at the input rate.
    ///
    /// When cached + creation exceed `input_tokens` (an adapter normalization bug) it warns,
    /// since the result under-bills, and does not panic: the counts are the provider's.
    pub fn cost_for(&self, usage: &Usage) -> CostUsd {
        let cached_plus_creation = usage
            .cached_input_tokens
            .saturating_add(usage.cache_creation_tokens);
        if cached_plus_creation > usage.input_tokens {
            tracing::warn!(
                input_tokens = usage.input_tokens,
                cached_input_tokens = usage.cached_input_tokens,
                cache_creation_tokens = usage.cache_creation_tokens,
                "Usage invariant violated: cached + creation > input — billing under-reported. \
                 Likely an adapter normalization bug; please file an issue.",
            );
        }
        let billable_input = usage.input_tokens.saturating_sub(cached_plus_creation);
        CostUsd(
            self.input_per_million.cost_of(billable_input as f64)
                + self.output_per_million.cost_of(usage.output_tokens as f64)
                + self
                    .cached_input_per_million
                    .cost_of(usage.cached_input_tokens as f64)
                + self
                    .cache_creation_per_million
                    .cost_of(usage.cache_creation_tokens as f64)
                + self
                    .thinking_per_million
                    .cost_of(usage.thinking_tokens as f64),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(v: f64) -> Rate {
        Rate::new(v).unwrap()
    }

    #[test]
    fn a_rate_is_a_finite_non_negative_number_up_to_the_cap() {
        assert!(Rate::new(0.0).is_ok());
        assert!(Rate::new(Rate::MAX).is_ok());
        for bad in [-0.01, f64::NAN, f64::INFINITY, Rate::MAX * 1.000_001] {
            assert!(Rate::new(bad).is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<Rate>("-1.0").is_err());
        assert_eq!(serde_json::from_str::<Rate>("2.5").unwrap(), rate(2.5));
    }

    #[test]
    fn provider_usage_with_more_cached_than_input_is_priced_without_a_panic() {
        let p = Pricing {
            input_per_million: rate(1.0),
            cached_input_per_million: rate(1.0),
            ..Pricing::default()
        };
        let u = Usage {
            input_tokens: 10,
            cached_input_tokens: 20,
            ..Usage::default()
        };
        let cost = std::panic::catch_unwind(|| p.cost_for(&u)).expect("cost_for panicked");
        assert!((cost.0 - 20.0 / 1_000_000.0).abs() < 1e-12, "{cost:?}");
    }

    #[test]
    fn the_largest_usage_at_the_highest_rate_costs_a_finite_amount() {
        let p = Pricing {
            input_per_million: rate(Rate::MAX),
            output_per_million: rate(Rate::MAX),
            cached_input_per_million: rate(Rate::MAX),
            cache_creation_per_million: rate(Rate::MAX),
            thinking_per_million: rate(Rate::MAX),
        };
        let u = Usage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            thinking_tokens: u64::MAX,
        };
        assert!(p.cost_for(&u).as_f64().is_finite());
    }

    #[test]
    fn merge_saturates() {
        let a = Usage {
            input_tokens: u64::MAX,
            ..Default::default()
        };
        let b = Usage {
            input_tokens: 5,
            ..Default::default()
        };
        assert_eq!(a.merge(b).input_tokens, u64::MAX);
    }

    #[test]
    fn pricing_includes_thinking_tokens_when_priced() {
        let p = Pricing {
            thinking_per_million: rate(30.0),
            ..Default::default()
        };
        let u = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            thinking_tokens: 1000,
        };
        // 1000 * 30 / 1e6 = 0.03
        assert!((p.cost_for(&u).0 - 0.03).abs() < 1e-9);
    }

    #[test]
    fn pricing_subtracts_cached_from_billable_input() {
        let p = Pricing {
            input_per_million: rate(10.0),
            output_per_million: rate(30.0),
            cached_input_per_million: rate(1.0),
            cache_creation_per_million: rate(12.5),
            thinking_per_million: Rate::ZERO,
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
