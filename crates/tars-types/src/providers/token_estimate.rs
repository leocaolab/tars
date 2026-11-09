//! Estimates how many tokens text takes without a tokenizer, for the checks that run before a
//! call: budgets, rate limits, the context window.

/// The UTF-8 bytes one token stands for: the ratio of English text under BPE tokenizers, not
/// measured per provider. Multi-byte scripts and code take more tokens per byte, so an estimate
/// built on it runs low for them.
pub const BYTES_PER_TOKEN: usize = 4;

/// Returns the tokens `bytes` of text are estimated to take, rounded up.
pub fn estimate_tokens(bytes: usize) -> u64 {
    // `usize` is at most 64 bits on every target tars builds for; saturating keeps an estimate
    // that gates a check from wrapping small.
    u64::try_from(bytes)
        .unwrap_or(u64::MAX)
        .div_ceil(BYTES_PER_TOKEN as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_token_of_text_counts_as_a_whole_one() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2);
    }
}
