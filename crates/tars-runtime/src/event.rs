//! [`AgentEvent`] — append-only event log unit (Doc 04 §3.2).
//!
//! Eight variants for M3 first cut. Each event is **small** by design
//! — large payloads (full LLM responses, image bytes, RAG context)
//! get stored separately and referenced by id once `ContentStore`
//! lands. Today we use plain `String` summaries; the struct shape
//! lets that field grow into a typed `ContentRef` later without
//! changing call sites.
//!
//! ## Why these eight, and not Doc 04's full set
//!
//! Doc 04 §3.2 lists ten variants including `CompensationExecuted`,
//! `LlmResponseCaptured` (separate from `StepCompleted`), and
//! `Checkpoint`. The split between `LlmResponseCaptured` /
//! `StepCompleted` matters when "we changed the parser, replay
//! against the raw bytes" is a real workflow. We don't have that
//! workflow yet, so the M3 first cut has a single
//! [`AgentEvent::LlmCallCaptured`] that records both intent + result
//! summaries. When the parser-rewind story becomes concrete we split
//! it and bump `payload` schema-version (currently zero — the
//! event_store's `user_version` covers schema evolution at the row
//! level; per-event versioning is a separate concern we're not
//! solving here).
//!
//! ## Forward-compat: `#[serde(other)]`
//!
//! Each variant is tagged `type` with snake_case names. New variants
//! land WITHOUT a `#[serde(other)]` catchall: a reader that doesn't
//! know about a new event type SHOULD fail loudly rather than
//! silently dropping it. Recovery code that processes events one-by-
//! one needs to know it skipped something.

use serde::{Deserialize, Serialize};

use tars_types::{ProviderId, TrajectoryId, Usage};

/// Unique key for an Agent step's external operations. Hash of
/// `(trajectory_id, step_seq, input_summary)` so replay can dedupe
/// LLM calls / tool invocations / DB writes. Doc 04 §3.2 invariant 3.
///
/// Stored inline on `StepStarted`; downstream operations carry it as
/// metadata when they hit external systems.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepIdempotencyKey(pub String);

impl StepIdempotencyKey {
    /// Compute from the standard inputs. The hash function is
    /// deliberately stable across versions — bump the prefix if you
    /// ever need to invalidate (same trick as `tars-cache`'s
    /// `hasher_version`).
    pub fn compute(traj: &TrajectoryId, step_seq: u32, input_summary: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"tars-runtime/step-idempotency/v1\0");
        h.update(traj.as_ref().as_bytes());
        h.update(b"\0");
        h.update(step_seq.to_le_bytes());
        h.update(b"\0");
        h.update(input_summary.as_bytes());
        let bytes = h.finalize();
        Self(format!("{bytes:x}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Append-only event log unit. See module docs for what's in / out.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    // ── Trajectory lifecycle ────────────────────────────────────────

    /// Trajectory came into existence. `parent` is set on branches
    /// (replan / fork / recovery); `None` on a root trajectory.
    /// `reason` is a free-form string today; will become a typed
    /// `BranchReason` enum (Doc 04 §3.1) when something cares about
    /// the discriminator.
    TrajectoryStarted {
        traj: TrajectoryId,
        parent: Option<TrajectoryId>,
        reason: String,
    },

    /// Trajectory finished successfully. `summary` is a short
    /// human-readable description — full result goes in the final
    /// `StepCompleted`'s output_summary or a future `ContentRef`.
    TrajectoryCompleted {
        traj: TrajectoryId,
        summary: String,
    },

    /// Trajectory paused (deadline pause / user suspend / waiting
    /// on an external input). `reason` is free-form like
    /// `TrajectoryStarted::reason`.
    TrajectorySuspended {
        traj: TrajectoryId,
        reason: String,
    },

    /// Trajectory was abandoned — by backtrack, budget exhaustion,
    /// deadline, explicit abort. Once written, no further events
    /// for this `traj` should appear (consumers SHOULD warn if they
    /// see one).
    TrajectoryAbandoned {
        traj: TrajectoryId,
        cause: String,
    },

    // ── Step lifecycle (intra-trajectory work units) ────────────────

    /// One agent step is starting. `step_seq` is monotonic
    /// per-trajectory (1-indexed). `agent` is a free-form id (e.g.
    /// `"orchestrator"`, `"worker:code_review"`); will become a
    /// typed `AgentRole` (Doc 04 §4) when the agent registry lands.
    /// `idempotency_key` lets replay dedupe external operations
    /// triggered by this step.
    StepStarted {
        traj: TrajectoryId,
        step_seq: u32,
        agent: String,
        idempotency_key: StepIdempotencyKey,
        input_summary: String,
    },

    /// Step finished successfully. `usage` aggregates LLM token cost
    /// for this step; downstream cost-attribution code reads the sum
    /// across all `StepCompleted` rows in a trajectory.
    StepCompleted {
        traj: TrajectoryId,
        step_seq: u32,
        output_summary: String,
        usage: Usage,
    },

    /// Step failed. `classification` is a short tag (e.g.
    /// `"retriable"`, `"permanent"`) so recovery code can decide
    /// without re-parsing the error string. Aligns with
    /// [`tars_types::ErrorClass`] but kept as a string today —
    /// the error itself isn't a `ProviderError` (could be a tool
    /// error, an agent-internal error, etc.).
    StepFailed {
        traj: TrajectoryId,
        step_seq: u32,
        error: String,
        classification: String,
    },

    // ── External-call captures (replay primitives) ──────────────────

    /// One LLM call within a step. Records what we asked + what we
    /// got back at a summary level. Doc 04 §3.2 envisages a separate
    /// `LlmResponseCaptured` variant carrying the *raw* response for
    /// parser-rewind replay; we'll split when that workflow exists.
    LlmCallCaptured {
        traj: TrajectoryId,
        step_seq: u32,
        provider: ProviderId,
        prompt_summary: String,
        response_summary: String,
        usage: Usage,
    },
}

impl AgentEvent {
    /// Trajectory id this event belongs to. Every variant carries
    /// one — exposed as a method so projections / filters don't have
    /// to match the full enum.
    pub fn trajectory_id(&self) -> &TrajectoryId {
        match self {
            Self::TrajectoryStarted { traj, .. }
            | Self::TrajectoryCompleted { traj, .. }
            | Self::TrajectorySuspended { traj, .. }
            | Self::TrajectoryAbandoned { traj, .. }
            | Self::StepStarted { traj, .. }
            | Self::StepCompleted { traj, .. }
            | Self::StepFailed { traj, .. }
            | Self::LlmCallCaptured { traj, .. } => traj,
        }
    }

    /// True iff this is a terminal event for the trajectory
    /// (Completed / Abandoned). Recovery scans use this to decide
    /// whether a trajectory is still active.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::TrajectoryCompleted { .. } | Self::TrajectoryAbandoned { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t() -> TrajectoryId {
        TrajectoryId::new("t1")
    }

    #[test]
    fn round_trip_through_json_preserves_variant() {
        let original = AgentEvent::TrajectoryStarted {
            traj: t(),
            parent: None,
            reason: "root".into(),
        };
        let v = serde_json::to_value(&original).unwrap();
        assert_eq!(v["type"], "trajectory_started");
        let back: AgentEvent = serde_json::from_value(v).unwrap();
        assert!(matches!(back, AgentEvent::TrajectoryStarted { .. }));
    }

    #[test]
    fn step_started_carries_idempotency_key() {
        let key = StepIdempotencyKey::compute(&t(), 1, "input");
        let ev = AgentEvent::StepStarted {
            traj: t(),
            step_seq: 1,
            agent: "orchestrator".into(),
            idempotency_key: key.clone(),
            input_summary: "input".into(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        // Transparent serde — key serializes as bare string, not
        // `{key: "..."}`.
        assert!(v["idempotency_key"].as_str().is_some());
        assert_eq!(v["idempotency_key"].as_str().unwrap(), key.as_str());
    }

    #[test]
    fn idempotency_key_is_deterministic() {
        let k1 = StepIdempotencyKey::compute(&t(), 1, "input");
        let k2 = StepIdempotencyKey::compute(&t(), 1, "input");
        assert_eq!(k1, k2, "same inputs → same key");
        let k3 = StepIdempotencyKey::compute(&t(), 1, "different");
        assert_ne!(k1, k3, "different input → different key");
        let k4 = StepIdempotencyKey::compute(&t(), 2, "input");
        assert_ne!(k1, k4, "different step_seq → different key");
    }

    #[test]
    fn idempotency_key_is_64_lowercase_hex() {
        let k = StepIdempotencyKey::compute(&t(), 1, "input");
        assert_eq!(k.as_str().len(), 64);
        assert!(k.as_str().chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn trajectory_id_extraction_works_for_every_variant() {
        let cases: Vec<AgentEvent> = vec![
            AgentEvent::TrajectoryStarted { traj: t(), parent: None, reason: "x".into() },
            AgentEvent::TrajectoryCompleted { traj: t(), summary: "x".into() },
            AgentEvent::TrajectorySuspended { traj: t(), reason: "x".into() },
            AgentEvent::TrajectoryAbandoned { traj: t(), cause: "x".into() },
            AgentEvent::StepStarted {
                traj: t(),
                step_seq: 1,
                agent: "a".into(),
                idempotency_key: StepIdempotencyKey("k".into()),
                input_summary: "x".into(),
            },
            AgentEvent::StepCompleted {
                traj: t(),
                step_seq: 1,
                output_summary: "x".into(),
                usage: Usage::default(),
            },
            AgentEvent::StepFailed {
                traj: t(),
                step_seq: 1,
                error: "x".into(),
                classification: "permanent".into(),
            },
            AgentEvent::LlmCallCaptured {
                traj: t(),
                step_seq: 1,
                provider: ProviderId::new("p"),
                prompt_summary: "x".into(),
                response_summary: "y".into(),
                usage: Usage::default(),
            },
        ];
        for ev in cases {
            assert_eq!(ev.trajectory_id(), &t());
        }
    }

    #[test]
    fn is_terminal_matches_completed_and_abandoned_only() {
        assert!(AgentEvent::TrajectoryCompleted { traj: t(), summary: "x".into() }.is_terminal());
        assert!(AgentEvent::TrajectoryAbandoned { traj: t(), cause: "x".into() }.is_terminal());
        assert!(!AgentEvent::TrajectoryStarted {
            traj: t(),
            parent: None,
            reason: "x".into(),
        }
        .is_terminal());
        assert!(!AgentEvent::TrajectorySuspended { traj: t(), reason: "x".into() }.is_terminal());
        assert!(!AgentEvent::StepCompleted {
            traj: t(),
            step_seq: 1,
            output_summary: "x".into(),
            usage: Usage::default(),
        }
        .is_terminal());
    }

    #[test]
    fn type_tags_use_snake_case() {
        // Pin the wire-format discriminator so a future #[serde]
        // attribute change doesn't silently break consumers.
        let cases = [
            (
                AgentEvent::TrajectoryStarted { traj: t(), parent: None, reason: "x".into() },
                "trajectory_started",
            ),
            (
                AgentEvent::StepCompleted {
                    traj: t(),
                    step_seq: 1,
                    output_summary: "x".into(),
                    usage: Usage::default(),
                },
                "step_completed",
            ),
            (
                AgentEvent::LlmCallCaptured {
                    traj: t(),
                    step_seq: 1,
                    provider: ProviderId::new("p"),
                    prompt_summary: "x".into(),
                    response_summary: "y".into(),
                    usage: Usage::default(),
                },
                "llm_call_captured",
            ),
        ];
        for (ev, expected_tag) in cases {
            let v = serde_json::to_value(&ev).unwrap();
            assert_eq!(
                v["type"], json!(expected_tag),
                "tag for {ev:?} must be {expected_tag}",
            );
        }
    }
}
