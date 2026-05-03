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

/// SHA256 hex of the system prompt that went into an LLM call.
/// Used by [`AgentEvent::LlmCallCaptured::system_prompt_hash`] to pin
/// the audit trail. **Plain SHA256 of the raw bytes** — no version
/// prefix — so an external auditor can trivially verify by hashing
/// the same prompt source (e.g. `sha256sum read_file.txt` matches
/// what the trajectory logged).
///
/// Returns `None` when `system` is `None`. Distinct from
/// `Some(sha256(""))` — the absence of a system prompt is a
/// different audit fact than "the system prompt was empty".
pub fn hash_system_prompt(system: Option<&str>) -> Option<String> {
    use sha2::{Digest, Sha256};
    let s = system?;
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    Some(format!("{:x}", h.finalize()))
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
    ///
    /// `system_prompt_hash` is the SHA256 hex of `req.system` (when
    /// present). This is the **audit pin** — given the trajectory
    /// log alone, an external auditor can independently verify
    /// "this LLM call used SHA256(...) as its system prompt", then
    /// match that hash against the prompts shipped in the binary
    /// (e.g. by hashing the `tars-tools/src/builtins/*.txt` files at
    /// the relevant git revision). Plain SHA256 of the raw bytes —
    /// no version prefix — so independent verification is trivial.
    /// `None` means no system prompt was sent (distinct from an
    /// empty-string prompt, which would still hash). TODO L-1
    /// enterprise follow-on.
    ///
    /// **Scope** — this hashes ONLY the system prompt, not the full
    /// request fingerprint (tools / structured_output schema / user
    /// turns). The system prompt is the highest-value audit target
    /// because it's the model's standing instructions; everything
    /// else is per-call data. A future "full request fingerprint"
    /// could be added when SOC 2 / ISO audits demand it.
    LlmCallCaptured {
        traj: TrajectoryId,
        step_seq: u32,
        provider: ProviderId,
        prompt_summary: String,
        response_summary: String,
        usage: Usage,
        /// SHA256 hex of `req.system`; `None` when the request
        /// carried no system prompt. See variant docs for audit
        /// rationale. Defaulted on deserialise so older event-store
        /// rows (before this field existed) continue to read.
        #[serde(default)]
        system_prompt_hash: Option<String>,
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

    // ── hash_system_prompt (audit pin) ────────────────────────────

    #[test]
    fn hash_system_prompt_returns_none_for_none_input() {
        assert_eq!(hash_system_prompt(None), None);
    }

    #[test]
    fn hash_system_prompt_distinguishes_none_from_empty_string() {
        // The audit fact "no system prompt was sent" differs from
        // "the system prompt was empty"; the hash must reflect that.
        let empty = hash_system_prompt(Some(""));
        assert!(empty.is_some(), "empty string still hashes");
        // Pin: SHA256("") is a well-known constant.
        assert_eq!(
            empty.as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        );
        assert_ne!(empty, hash_system_prompt(None));
    }

    #[test]
    fn hash_system_prompt_is_deterministic() {
        let a = hash_system_prompt(Some("You are a planner."));
        let b = hash_system_prompt(Some("You are a planner."));
        assert_eq!(a, b);
    }

    #[test]
    fn hash_system_prompt_changes_on_prompt_change() {
        let a = hash_system_prompt(Some("You are a planner."));
        let b = hash_system_prompt(Some("You are a critic."));
        assert_ne!(a, b);
    }

    #[test]
    fn hash_system_prompt_format_is_64_lowercase_hex() {
        let h = hash_system_prompt(Some("anything")).unwrap();
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn hash_system_prompt_matches_external_sha256() {
        // The whole point of the no-version-prefix design: an
        // external auditor can verify by running `sha256sum`. Pin a
        // known value so accidental wrapping (e.g. someone adding a
        // version prefix) breaks this test loudly.
        // SHA256("hello\n") = 5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03
        // We hash without the newline:
        // SHA256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        assert_eq!(
            hash_system_prompt(Some("hello")).as_deref(),
            Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"),
        );
    }

    #[test]
    fn llm_call_captured_with_hash_round_trips_through_json() {
        let ev = AgentEvent::LlmCallCaptured {
            traj: t(),
            step_seq: 1,
            provider: ProviderId::new("p"),
            prompt_summary: "x".into(),
            response_summary: "y".into(),
            usage: Usage::default(),
            system_prompt_hash: hash_system_prompt(Some("you are a planner")),
        };
        let v = serde_json::to_value(&ev).unwrap();
        // Field is present + populated on serialize.
        assert!(v["system_prompt_hash"].is_string());
        let back: AgentEvent = serde_json::from_value(v).unwrap();
        match back {
            AgentEvent::LlmCallCaptured { system_prompt_hash: Some(h), .. } => {
                assert_eq!(h.len(), 64);
            }
            other => panic!("expected LlmCallCaptured with hash, got {other:?}"),
        }
    }

    #[test]
    fn llm_call_captured_round_trips_old_payload_without_hash_field() {
        // Migration safety: old event-store rows (written before
        // system_prompt_hash existed) must still parse. The serde
        // default lets them deserialise as None.
        let v = json!({
            "type": "llm_call_captured",
            "traj": "t1",
            "step_seq": 1,
            "provider": "p",
            "prompt_summary": "x",
            "response_summary": "y",
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "thinking_tokens": 0,
            },
        });
        let back: AgentEvent = serde_json::from_value(v).expect("old shape must still parse");
        match back {
            AgentEvent::LlmCallCaptured { system_prompt_hash, .. } => {
                assert!(system_prompt_hash.is_none());
            }
            other => panic!("expected LlmCallCaptured, got {other:?}"),
        }
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
                system_prompt_hash: None,
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
                    system_prompt_hash: None,
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
