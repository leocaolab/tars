//! # tars-agent2 — the coalgebra runtime
//!
//! **Core thesis:** an agent is a **coalgebra** — *one process*, a *single decide-emit
//! step function*. It does not evaluate to a value; it *behaves*, unfolding intents in
//! response to what it observes. It holds **no persistent state** — state lives in the
//! **world**, not in the agent and not in its history. The agent is a disposable
//! throwaway (no `Drop`, safe to kill and replay).
//!
//! **The second thing:** an agent needs a **god-program** beside it — the runtime — and
//! that thing is an **algebra**. It *folds* the agent's unfold (executes effects), *renders*
//! the world into scoped views, *memoizes*, *pages* to fit the window, and **terminates**:
//! it drives the world to a **fixed point** (`f(world) = world`, the diff is empty), or runs
//! out of fuel. Termination is a fixed point, never the agent certifying completeness.
//!
//! **The reconcile loop** (the hylomorphism — the algebra cranking the coalgebra):
//! `Diff (desired − actual) → agent decides → Check (delay, don't reject) → Push (effect)
//! → Validate → repeat until the diff is empty (converged) or fuel runs out.`
//!
//! **The load-bearing constraint:** all of this works to the degree the **Diff is cheap and
//! deterministic**. Make the desired state a set of runnable **checks** (build / test / lint /
//! a rendering oracle) so a real fixed point exists. Where the diff is a noisy judgment
//! (an LLM re-review), the loop oscillates and no fixed point is reachable.
//!
//! Grounded in `tars-internal/docs/architecture/agent/` 11 (coalgebra), 13 (world model),
//! 14 (the benchmark that validated it: v11 world-native won). The LLM-driven agent decides
//! through [`tars_pipeline::LlmService`] — never raw HTTP. `tars-agent` is untouched.

pub mod agent; // the coalgebra: Agent = decide-emit step fn; Step, Intent
pub mod world; // the world: Component (render + handlers + onUpdate + version marks); World
pub mod diff; // Diff = desired − actual; Check (the deterministic oracle); converged = empty
pub mod render; // render a big world: unfold / fold / emphasize; the View a bound agent sees
pub mod effect; // effect = an error-reducing actuation toward desired (the operate surface)
pub mod runtime; // the god-program (algebra): the reconcile loop → fixed point OR fuel
pub mod components; // a concrete reference world: File (versioned) + ShellCheck (deterministic Diff)
pub mod llm; // the LLM-driven agent, built on tars_pipeline::LlmService
pub mod review; // the map-reduce code reviewer: Finding + LlmService construction + parse

pub use agent::{Agent, Intent, Step, Wake};
pub use components::{File, ShellCheck};
pub use diff::{Check, CheckResult, Gap, RedCheck, Spec};
pub use effect::Observation;
pub use llm::LlmAgent;
pub use render::{CompView, View};
pub use review::{Backend, Finding};
pub use runtime::{Outcome, Runtime};
pub use world::{CompId, Component, Version, World};
