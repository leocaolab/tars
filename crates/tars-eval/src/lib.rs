//! The machinery that tests and scores tars: cassette diff, invariants, eval, judges, metamorphic
//! relations, trajectory matching.
//!
//! It sits over the transport (`tars-types` and `tars-llm`'s `LlmService`) and does not
//! know the agent runtime.

#![deny(clippy::print_stdout, clippy::print_stderr)]

pub mod arg_judge;
pub mod cassette_diff;
pub mod check;
pub mod eval;
pub mod judge;
pub mod judge_stats;
pub mod metamorphic;
#[cfg(test)]
mod test_support;
pub mod trajectory_match;

pub use arg_judge::{ArgEquivalenceJudge, args_match_judged};
pub use check::{CheckOutcome, CheckRunner, Invariant, MembershipInvariant, ValidatorInvariant};
pub use judge::{
    DEFAULT_JUDGE_PROMPT, Judge, JudgeError, LlmJudge, SharedProvider, SharedVendor, VendorCheck,
    check_providers, check_vendors, run_judge_pass,
};
pub use judge_stats::{
    DiscordantPairs, JudgeFailure, JudgeItem, JudgeReport, JudgeVerdict, JudgedItem, McNemarTest,
    UnjudgedItem, mcnemar,
};
pub use metamorphic::{
    DeleteSubstringMutation, DirectionalRelation, GoldenMatch, InvarianceRelation,
    MetamorphicRelation, Mutation, MutationVerdict, mutation_caught,
};
pub use trajectory_match::{MatchMode, ToolStep};

/// Returns `s` cut to `max` chars, with `…` appended when it was cut.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}
