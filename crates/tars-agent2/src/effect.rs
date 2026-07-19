//! Effects and their observations.
//!
//! An **effect** is an actuation toward the desired state: the god-program takes an
//! [`crate::agent::Intent`], dispatches it to the named component's handler, and gets back an
//! [`Observation`] — the *source-truth* record of what actually happened. The producer (the
//! handler) writes its own output at the moment it produces it; the runtime never re-derives a
//! reason from somewhere else.
//!
//! **On failure we carry the truth, never a sentinel.** A handler that can't parse its args
//! returns [`Observation::Failed`] holding the *raw args* and the *real error string*, so the
//! next [`crate::render::View`] shows the decider exactly what it sent and why it bounced —
//! not an opaque `parse_failed` token.

use crate::world::{CompId, Version};

/// What actually happened when the world applied one [`crate::agent::Intent`]. Fed back into
/// the next rendered [`crate::render::View`] so the agent observes the consequence of its move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    /// The handler ran and mutated the component. Carries the component's *new* version
    /// (its `onUpdate` bump) and the fresh render, so `operate → observe the new render`
    /// closes within a single crank.
    Applied {
        component: CompId,
        handler: String,
        new_version: Version,
        render: String,
    },
    /// The handler rejected the args (bad JSON, unknown handler, domain error). Carries the
    /// **raw args** and the **real error** — the truth the decider needs, not a sentinel.
    Failed {
        component: CompId,
        handler: String,
        raw_args: String,
        error: String,
    },
}

impl Observation {
    /// True iff the effect landed (the world moved).
    pub fn is_applied(&self) -> bool {
        matches!(self, Observation::Applied { .. })
    }

    /// A one-line human-readable rendering for the next View. Failures surface the real
    /// error and a truncated echo of the raw args — never a masked token.
    pub fn summary(&self) -> String {
        match self {
            Observation::Applied {
                component,
                handler,
                new_version,
                ..
            } => format!("OK  {component}.{handler} → v{new_version}"),
            Observation::Failed {
                component,
                handler,
                raw_args,
                error,
            } => {
                let raw = truncate(raw_args, 160);
                format!("ERR {component}.{handler}: {error} | raw args: {raw}")
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}
