//! The **render** — the scoped [`View`] a bound agent observes each crank.
//!
//! The god-program renders the world + the current diff into a `View` and hands the agent a
//! `&View`. The agent reads it and emits — it never holds the world. Because the render is
//! recomputed from current state every crank (the `onUpdate` contract, doc 14 finding E),
//! `operate → observe the new render → operate` closes within a turn and the agent never has to
//! reconstruct "where am I / what's left" from a transcript.
//!
//! This first cut renders the *whole* scope (unfold everything). The demand-driven
//! unfold/fold/emphasize paging of doc 13 §2.2 is the next lever (fold out-of-focus components
//! to a reference, emphasize the diff) — it changes *how much* of the view is shown, not the
//! `View`'s contract, so it slots in behind this type without touching the agent.

use crate::diff::Gap;
use crate::effect::Observation;
use crate::world::{CompId, Version};

/// One component's slice of the view: its id, current version, render, and operate surface.
#[derive(Clone, Debug)]
pub struct CompView {
    pub id: CompId,
    pub version: Version,
    pub render: String,
    pub handlers: Vec<String>,
}

/// What a bound agent sees this crank: the rendered components, the current **gap** (the red
/// checks = what is left to make green — the work-list), and the observations from the previous
/// crank (source-truth, so the agent sees the consequence of its last move — including any
/// failure carrying its raw args).
#[derive(Clone, Debug, Default)]
pub struct View {
    pub components: Vec<CompView>,
    pub gap: Gap,
    pub last: Vec<Observation>,
}

impl View {
    /// True iff the diff is empty — the world has converged. The agent may read this to decide
    /// to [`crate::agent::Step::ProposeHalt`], but the god-program owns the real check.
    pub fn converged(&self) -> bool {
        self.gap.is_empty()
    }

    /// A compact plain-text rendering of the whole view — the prompt body an [`crate::llm::LlmAgent`]
    /// feeds to the model. Deterministic (components are in id order).
    pub fn to_prompt(&self) -> String {
        let mut s = String::new();

        s.push_str("## Gap (red checks — the work left; empty = done)\n");
        if self.gap.is_empty() {
            s.push_str("(none — the world has converged)\n");
        } else {
            for r in &self.gap.red {
                s.push_str(&format!("- [{}] {}\n", r.check_id, r.detail));
            }
        }

        if !self.last.is_empty() {
            s.push_str("\n## Result of your last actions\n");
            for o in &self.last {
                s.push_str(&format!("- {}\n", o.summary()));
            }
        }

        s.push_str("\n## World (components you can observe and operate)\n");
        for c in &self.components {
            s.push_str(&format!(
                "### {} (v{}) — handlers: {}\n",
                c.id,
                c.version,
                c.handlers.join(", ")
            ));
            s.push_str(&c.render);
            if !c.render.ends_with('\n') {
                s.push('\n');
            }
        }
        s
    }
}
