//! The **god-program** — the algebra that folds the agent's unfold and drives the world to a
//! **fixed point**. This is the `anneal` / reconcile loop of doc 11 §3.2:
//!
//! ```text
//! loop {
//!     if converged(world, spec)  → Converged        // fixed point: gap empty
//!     if fuel exhausted          → Exhausted        // give up on a residual that won't settle
//!     view  = render(world, spec, last)             // Diff = desired − actual, into a scoped view
//!     step  = agent.step(&view)                     // the coalgebra decides (may call an LLM)
//!     match step {
//!         Emit(intents) → for i: obs = world.apply(i)   // Check (admit) → Push (effect) → Validate
//!         ProposeHalt   → verify against the world; accept only if truly converged
//!         Park(wake)    → suspend (a reply outlives this run)
//!     }
//! }
//! ```
//!
//! **The law (doc 11 §3.1):** termination is a fixed point (the world settled) OR fuel — never
//! the agent certifying completeness. `ProposeHalt` only *accelerates* the check; the runtime
//! still verifies the gap against the world and *refuses to lie* while a check is still red.

use crate::agent::{Agent, Step, Wake};
use crate::diff::{Gap, Spec};
use crate::effect::Observation;
use crate::render::{CompView, View};
use crate::world::World;

/// How the reconcile loop ended.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Reached the fixed point: the gap is empty. `iters` = cranks spent.
    Converged { iters: u32 },
    /// Ran out of fuel with a residual gap that would not settle. Carries the last gap so the
    /// caller sees exactly what stayed red (never a masked "failed").
    Exhausted { iters: u32, gap: Gap },
    /// The agent suspended on something outliving this run (a child result, a human reply).
    Parked { iters: u32, wake: Wake },
}

impl Outcome {
    pub fn converged(&self) -> bool {
        matches!(self, Outcome::Converged { .. })
    }
}

/// The god-program. Holds the fuel budget; owns the loop, the render, the effect execution, and
/// the termination decision. The agent and the world are borrowed per run.
pub struct Runtime {
    /// Max cranks before giving up on a residual gap. The "fuel" half of the termination law.
    fuel: u32,
}

impl Runtime {
    pub fn new(fuel: u32) -> Self {
        Self { fuel }
    }

    /// Render the current world + diff into the scoped [`View`] the agent observes.
    fn render(world: &World, spec: &Spec, last: Vec<Observation>) -> View {
        let components = world
            .components()
            .map(|c| CompView {
                id: c.id(),
                version: c.version(),
                render: c.render(),
                handlers: c.handlers(),
            })
            .collect();
        View {
            components,
            gap: spec.gap(world),
            last,
        }
    }

    /// Drive `world` toward `spec` using `agent`, until the fixed point (gap empty) or fuel runs
    /// out. This is the hylomorphism: the algebra (this loop) cranking the coalgebra (`agent`).
    pub async fn anneal(
        &self,
        world: &mut World,
        spec: &Spec,
        agent: &mut dyn Agent,
    ) -> Outcome {
        let mut last: Vec<Observation> = Vec::new();

        for iters in 0..self.fuel {
            // Termination check #1: the fixed point. Owned by the god-program, computed from the
            // world (never the agent's self-report).
            if world.converged(spec) {
                return Outcome::Converged { iters };
            }

            let view = Self::render(world, spec, std::mem::take(&mut last));
            let step = agent.step(&view).await;

            match step {
                Step::Emit(intents) => {
                    // Push each effect; record the source-truth observation for the next render.
                    for intent in &intents {
                        last.push(world.apply(intent));
                    }
                }
                Step::ProposeHalt => {
                    // The agent says "no productive move left." Verify against the world: accept
                    // only if the gap is truly empty; otherwise refuse to lie and keep annealing.
                    if world.converged(spec) {
                        return Outcome::Converged { iters };
                    }
                    // Not converged but the agent is stuck: nothing more it will do this run.
                    // Return the residual honestly rather than spinning to burn fuel.
                    return Outcome::Exhausted {
                        iters,
                        gap: spec.gap(world),
                    };
                }
                Step::Park(wake) => {
                    return Outcome::Parked { iters, wake };
                }
            }
        }

        // Fuel exhausted. Report the residual gap honestly — one last convergence check first in
        // case the final crank's effects closed it.
        if world.converged(spec) {
            return Outcome::Converged { iters: self.fuel };
        }
        Outcome::Exhausted {
            iters: self.fuel,
            gap: spec.gap(world),
        }
    }
}
