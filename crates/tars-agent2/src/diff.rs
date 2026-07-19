//! The **Diff** — `desired − actual`, expressed as a set of runnable [`Check`]s.
//!
//! `Spec` is the desired state = a set of Checks. Running the spec against a [`World`] yields a
//! [`Gap`] = the **red** checks = the part of the desired state not yet met. `gap.is_empty()`
//! **is** the fixed point: the world has converged (doc 11 §3.1 — "termination is a fixed point,
//! not a completeness claim").
//!
//! **The load-bearing constraint (doc 14 §3.7, laws 6 & 8):** the loop reaches a fixed point
//! only to the degree each Check is a **cheap deterministic oracle** — a build, a test, a lint,
//! a render comparison. A Check built from a noisy LLM re-judgment re-flags inputs it just
//! passed (measured: even at temp=0) so the loop *oscillates* and no fixed point is reachable.
//! The winning arc demo (v11) drove its loop on exactly such a noisy check and, honestly,
//! never reached 0 — it only *bounded* the oscillation via memoization. The determinism lives
//! HERE, in the Check, which is why [`crate::components::ShellCheck`] shells out to a real
//! command (`cargo test`) whose exit code is the verdict.

use crate::world::World;

/// The verdict of one [`Check`] against the current world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
    /// Desired: this slice of the spec is met.
    Green,
    /// Not-yet-desired: carries the real detail (the failing command's output, the diff) so the
    /// agent's next [`crate::render::View`] shows *why* it is red — never a masked sentinel.
    Red { detail: String },
}

impl CheckResult {
    pub fn is_green(&self) -> bool {
        matches!(self, CheckResult::Green)
    }
}

/// A deterministic oracle over the world. Evaluating it must be a pure function of the world's
/// current state (so a `Green` this crank stays `Green` next crank unless the world moved) —
/// that determinism is the precondition for the fixed point to exist.
pub trait Check: Send + Sync {
    /// Stable, low-cardinality id — the diff identity / memo key for this check.
    fn id(&self) -> String;
    /// Evaluate the check against the current world. Deterministic in the world's state.
    fn eval(&self, world: &World) -> CheckResult;
}

/// The **desired state**: a set of checks that must all be green. Diff = desired − actual =
/// the red subset (the [`Gap`]).
#[derive(Default)]
pub struct Spec {
    checks: Vec<Box<dyn Check>>,
}

impl Spec {
    pub fn new() -> Self {
        Self { checks: Vec::new() }
    }

    /// Add a check to the desired state (builder-style).
    pub fn with(mut self, check: impl Check + 'static) -> Self {
        self.checks.push(Box::new(check));
        self
    }

    /// Number of checks in the spec.
    pub fn len(&self) -> usize {
        self.checks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    /// Compute the **Diff**: run every check against the world, collect the red ones. An empty
    /// `Gap` = the fixed point = converged. This is the only termination authority (doc 11 §3.1
    /// — never the agent's self-report).
    pub fn gap(&self, world: &World) -> Gap {
        let red = self
            .checks
            .iter()
            .filter_map(|c| match c.eval(world) {
                CheckResult::Green => None,
                CheckResult::Red { detail } => Some(RedCheck {
                    check_id: c.id(),
                    detail,
                }),
            })
            .collect();
        Gap { red }
    }
}

/// One failing check: the check's id + the real detail of why it is red.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedCheck {
    pub check_id: String,
    pub detail: String,
}

/// The **gap** = the red checks = `desired − actual`. Empty = fixed point = converged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gap {
    pub red: Vec<RedCheck>,
}

impl Gap {
    /// The fixed-point predicate. `gap.is_empty()` ⇔ the world satisfies the whole spec.
    pub fn is_empty(&self) -> bool {
        self.red.is_empty()
    }

    /// How far from converged — the count of red checks.
    pub fn len(&self) -> usize {
        self.red.len()
    }
}
