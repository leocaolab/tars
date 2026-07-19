//! The agent — a **coalgebra**. One process, a single decide-emit step function.
//!
//! The agent holds no persistent state: it borrows a rendered [`View`] of (its slice of)
//! the world and emits intents. State lives in the world (see [`crate::world`]); the agent
//! is a disposable throwaway (no `Drop`, safe to kill and replay). This whole module is the
//! coalgebra's `unfold` side.
//!
//! [`Agent::step`] is **async** on purpose: the one production agent — [`crate::llm::LlmAgent`]
//! — decides by calling an LLM through [`tars_pipeline::LlmService`], which is async. A pure
//! in-process agent (see the reconcile test's stub) simply doesn't `.await` anything.

use async_trait::async_trait;

use crate::render::View;
use crate::world::CompId;

/// An **intent** = a requested effect (the *operate* surface): which component's handler,
/// with what args. `args` is the raw argument *string* the decider produced (the LLM's
/// possibly-malformed tool-call JSON) — parse and validate at the boundary (in the handler);
/// on failure, the failure (carrying the raw args) IS the [`crate::effect::Observation`]
/// handed back, never a swallowed error or a sentinel token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    pub component: CompId,
    pub handler: String,
    pub args: String,
}

impl Intent {
    pub fn new(
        component: impl Into<CompId>,
        handler: impl Into<String>,
        args: impl Into<String>,
    ) -> Self {
        Self {
            component: component.into(),
            handler: handler.into(),
            args: args.into(),
        }
    }
}

/// A resume key for a [`Step::Park`]ed step — the continuation is re-derived from the world,
/// not a held stack. (Reserved for the human-in-the-loop / child-result path; the reconcile
/// loop treats a `Park` as a suspend point.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wake(pub String);

/// The output of one decide-emit step — the coalgebra's unfold for this crank.
#[derive(Clone, Debug)]
pub enum Step {
    /// Emit effect-requests (intents) to apply toward the desired state.
    Emit(Vec<Intent>),
    /// Inside hint: "I have no productive move left." The god-program still *verifies* the
    /// fixed point against the world (law: never trust the agent's self-report) — this only
    /// *accelerates* the check, it does not decide termination. See [`crate::runtime`].
    ProposeHalt,
    /// Suspend until an answer outliving this run arrives (a child's result, a human reply,
    /// an oracle). Resume by [`Wake`] key.
    Park(Wake),
}

/// The agent **is** this: a single decide-emit step over an observed [`View`]. Nothing more.
///
/// Deliberately *not* a builder of sub-agents, not a returned plan, not a taxonomy of roles —
/// one process. Plans, roles, and sub-work are *data + tools* in the world, not other agents.
#[async_trait]
pub trait Agent: Send {
    async fn step(&mut self, view: &View) -> Step;
}
