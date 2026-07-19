//! The world — where **all state lives** (not in the agent, not in history). A set of
//! **components**, each with a render + handlers + an `onUpdate` (a version bump on write).
//!
//! This is the piece that makes the agent work: because state is in the world, the agent reads
//! the *current* render cheaply and history shrinks to the action log. Skip the world model and
//! the agent must reconstruct state from an ever-growing transcript (the shipping-agent failure
//! mode — doc 14 §3.7 finding H).

use std::collections::BTreeMap;

use crate::agent::Intent;
use crate::diff::Spec;
use crate::effect::Observation;

pub type CompId = String;

/// A content-hash **version** — one field, three jobs (doc 13 §2.1):
/// - **identity** for reconciliation (the render key, like a React `key`);
/// - **memo key** — skip re-deriving an unchanged component (kills the review oscillation);
/// - **concurrency CAS token** — a write is compare-and-swap on the version (MVCC; no lock).
pub type Version = u64;

/// A component: the observable + operable unit of the world.
///
/// `render` is the observable view (doc 13 §2.1 clause 1 — a template, or a default-by-shape
/// render). `handle` is the operate surface — a named handler that mutates the component; on
/// every mutation the impl MUST bump its [`Component::version`] (the `onUpdate` contract, doc 14
/// finding E), so the render is never a carried snapshot: it is recomputed from current state
/// and `operate → observe the new render → operate` closes within a single crank.
pub trait Component: Send {
    fn id(&self) -> CompId;

    /// The current version (content-hash). Bumped by any mutation = `onUpdate`.
    fn version(&self) -> Version;

    /// The observable view (one-way, free). A template or the default-by-shape render.
    fn render(&self) -> String;

    /// The operate surface: named handlers the agent may invoke. This IS "the tool list the
    /// model sees" for this component (doc 15 — actions auto-derived from the component).
    fn handlers(&self) -> Vec<String>;

    /// Apply a named handler with the decider's raw `args` string. The handler parses/validates
    /// at the boundary and, on success, mutates + bumps the version. On failure it returns
    /// [`Observation::Failed`] carrying the **raw args + the real error** (doc 13 §2.2 — "the
    /// failure, carrying the raw args, IS the observation handed back", never a sentinel).
    fn handle(&mut self, handler: &str, args: &str) -> Observation;
}

/// The world: components + (eventually) the derived memoized views over them. Findings etc. are
/// *derived views* over the components, not independent stores (doc 13 §2.1 clause 4).
#[derive(Default)]
pub struct World {
    components: BTreeMap<CompId, Box<dyn Component>>,
}

impl World {
    pub fn new() -> Self {
        Self {
            components: BTreeMap::new(),
        }
    }

    /// Insert / replace a component (builder-style).
    pub fn with(mut self, component: impl Component + 'static) -> Self {
        self.components
            .insert(component.id(), Box::new(component));
        self
    }

    /// Borrow a component by id (for a [`crate::diff::Check`] to read its state).
    pub fn get(&self, id: &str) -> Option<&dyn Component> {
        self.components.get(id).map(|c| c.as_ref())
    }

    /// Iterate the components in id order (deterministic — used by the renderer).
    pub fn components(&self) -> impl Iterator<Item = &dyn Component> {
        self.components.values().map(|c| c.as_ref())
    }

    /// The **fixed point**: the world has converged when the spec's diff (gap) is empty
    /// (doc 11 §3.1). This is *the* termination condition — not the agent's say-so.
    pub fn converged(&self, spec: &Spec) -> bool {
        spec.gap(self).is_empty()
    }

    /// Apply an intent — an **effect**, an actuation toward the desired state. Dispatches to the
    /// named component's handler; the component mutates and bumps its version (`onUpdate`). The
    /// returned [`Observation`] is the source-truth record fed back into the next render. An
    /// intent naming an unknown component fails loud (carrying the raw args), never silently.
    pub fn apply(&mut self, intent: &Intent) -> Observation {
        match self.components.get_mut(&intent.component) {
            Some(component) => component.handle(&intent.handler, &intent.args),
            None => Observation::Failed {
                component: intent.component.clone(),
                handler: intent.handler.clone(),
                raw_args: intent.args.clone(),
                error: format!("no such component `{}` in world", intent.component),
            },
        }
    }
}
