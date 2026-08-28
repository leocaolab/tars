//! tars-harness — the `tars` command line, and how tars is tested and scored.
//!
//! Everything here sits *over* the transport (`tars-types` +
//! `tars-pipeline::LlmService`) and knows nothing about the agent runtime. Two
//! halves share the crate because they are used together and separately would
//! only import each other:
//!
//! The name is `harness`, not `eval`: eval is one module here, and more than
//! half of what is in this crate is testing machinery that no eval run touches.

pub mod arg_judge;
pub mod bless;
pub mod cassette_diff;
pub mod check;
pub mod cli;
pub mod eval;
pub mod judge;
pub mod judge_stats;
pub mod metamorphic;
pub mod trajectory_match;

pub use arg_judge::{ArgEquivalenceJudge, args_match_judged};
pub use check::{CheckResult, CheckRunner, Invariant, MembershipInvariant, ValidatorInvariant};
pub use judge::{
    DEFAULT_JUDGE_PROMPT, Judge, JudgeError, LlmJudge, ensure_anti_incest, run_judge_pass,
};
pub use judge_stats::{JudgeItem, JudgeReport, JudgeVerdict, JudgedItem, McNemarResult, mcnemar};
pub use metamorphic::{
    DeleteSubstringMutation, DirectionalRelation, GoldenMatch, InvarianceRelation,
    MetamorphicRelation, Mutation, MutationVerdict, mutation_caught,
};
pub use trajectory_match::{MatchMode, ToolStep};

pub use bless::{Assert, Bless, BlessError, BlessOutcome, Codec, Drift, MatchTier};
