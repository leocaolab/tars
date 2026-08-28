//! tars-harness — the `tars` command line, and how tars is tested and scored.
//!
//! The binary lives here (`src/bin/tars.rs`) with its commands under [`cli`].
//! It used to be a crate of its own, which bought a `Cargo.toml` and cost a
//! boundary: the harness's own flags sat on one side of it and the machinery
//! they drive on the other, so adding a flag meant editing two crates.
//!
//! Everything here sits *over* the transport (`tars-types` +
//! `tars-pipeline::LlmService`) and knows nothing about the agent runtime. Two
//! halves share the crate because they are used together and separately would
//! only import each other:
//!
//! **Testing.** What you reach for to hold a change still.
//!
//! - [`bless`]: the golden-file approval loop — committed field-level
//!   assertions, drift reported per field rather than as one failed compare.
//! - [`cassette_diff`]: what changed between a recorded request and the live
//!   one, located — the answer a cassette MISS owes its reader.
//! - [`check`]: deterministic invariants over one request/response
//!   ([`Invariant`], [`CheckRunner`], membership / validator invariants).
//!
//! **Scoring.** What you reach for to say whether a change was an improvement.
//!
//! - [`eval`]: the corpus-replay engine behind `tars eval` — replay, judge,
//!   diff, bless, in one pass over a case directory.
//! - [`judge`]: LLM-as-judge — the [`Judge`] trait, [`LlmJudge`],
//!   [`run_judge_pass`], anti-incest guard, default prompt.
//! - [`judge_stats`]: pure statistics over judge verdicts — precision,
//!   Wilson CI, unsure-rate, McNemar's paired test.
//! - [`arg_judge`]: LLM judge for tool-call *argument* equivalence.
//! - [`metamorphic`]: metamorphic relations + mutation catch-rate for
//!   golden-free / self-consistency testing.
//! - [`trajectory_match`]: pure tool-trajectory scoring (names / args).
//!
//! The name is `harness`, not `eval`: eval is one module here, and more than
//! half of what is in this crate is testing machinery that no eval run touches.
//!
//! - [`cli`]: the arg definitions for every `tars` subcommand, and the thin
//!   dispatch behind each one. The machinery they call lives in the modules
//!   above and in the layers below.

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
