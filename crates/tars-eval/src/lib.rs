//! tars-eval — eval / quality machinery for tars.
//!
//! Scoring, judging, and testing primitives that sit *over* the tars
//! transport (`tars-types` + `tars-pipeline::LlmService`) but have
//! nothing to do with the agent *runtime*. Split out of the stale
//! `tars-runtime` crate so the eval story stands on its own:
//!
//! - [`judge`]: LLM-as-judge — the [`Judge`] trait, [`LlmJudge`],
//!   [`run_judge_pass`], anti-incest guard, default prompt.
//! - [`judge_stats`]: pure statistics over judge verdicts — precision,
//!   Wilson CI, unsure-rate, McNemar's paired test.
//! - [`arg_judge`]: LLM judge for tool-call *argument* equivalence.
//! - [`check`]: cheap deterministic invariants over a request/response
//!   ([`Invariant`], [`CheckRunner`], membership / validator invariants).
//! - [`metamorphic`]: metamorphic relations + mutation catch-rate for
//!   golden-free / self-consistency testing.
//! - [`trajectory_match`]: pure tool-trajectory scoring (names / args).

pub mod arg_judge;
pub mod bless;
pub mod check;
pub mod judge;
pub mod judge_stats;
pub mod metamorphic;
pub mod trajectory_match;

pub use arg_judge::{ArgEquivalenceJudge, args_match_judged};
pub use check::{CheckResult, CheckRunner, Invariant, MembershipInvariant, ValidatorInvariant};
pub use judge::{
    DEFAULT_JUDGE_PROMPT, Judge, JudgeError, LlmJudge, ensure_anti_incest, run_judge_pass,
};
pub use judge_stats::{
    JudgeItem, JudgeReport, JudgeVerdict, JudgedItem, McNemarResult, mcnemar,
};
pub use metamorphic::{
    DeleteSubstringMutation, DirectionalRelation, GoldenMatch, InvarianceRelation,
    MetamorphicRelation, Mutation, MutationVerdict, mutation_caught,
};
pub use trajectory_match::{MatchMode, ToolStep};

pub use bless::{Assert, Bless, BlessError, BlessOutcome, Codec, Drift, MatchTier};
