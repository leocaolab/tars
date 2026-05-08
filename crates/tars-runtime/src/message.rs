//! [`AgentMessage`] — typed envelope for inter-agent communication
//! (Doc 04 §4.2).
//!
//! ## Why typed messages and not plain JSON
//!
//! Doc 04 explicitly says **禁止纯文本互喷** ("no plain text inter-agent
//! communication"): an Orchestrator hands a Worker a typed plan, the
//! Worker reports back with a typed result, the Critic emits a typed
//! verdict. Each variant carries the structured fields downstream
//! consumers actually need; no string-parsing in the orchestration
//! loop, no "did the Worker mean Approve or Approve!"-style
//! ambiguity in the Critic feedback.
//!
//! This module is the contract; the orchestration loop (B-4 in
//! TODO.md) is the immediate consumer.
//!
//! ## Scope of the first cut
//!
//! Doc 04 §4.2 enumerates a richer set than what's here. We ship the
//! **4 variants with concrete near-term consumers**:
//!
//! - [`AgentMessage::PlanIssued`] — Orchestrator → Worker[s]. Wraps
//!   the typed [`crate::Plan`] [`OrchestratorAgent::plan`] already
//!   produces.
//! - [`AgentMessage::PartialResult`] — Worker → Orchestrator/
//!   Aggregator. Reports back what the Worker did for one plan step,
//!   plus a confidence signal the orchestration loop or Critic can
//!   weigh.
//! - [`AgentMessage::Verdict`] — Critic → Orchestrator. Approve /
//!   Reject (with reason) / Refine (with suggestions). The
//!   replanning trigger.
//! - [`AgentMessage::NeedsClarification`] — any agent → upstream.
//!   Lets an agent surface "I can't make progress without X"
//!   instead of guessing.
//!
//! Skipped, no consumer yet:
//!   - WorkerOnboarded / Heartbeat / status-streaming variants —
//!     operational instrumentation; metrics will cover this when
//!     `tars-melt` grows them (B-8).
//!   - Tool dispatch envelopes — those go through the Tool registry
//!     directly (B-9), not via AgentMessage.
//!   - Aggregator-specific ensemble messages — wait for an
//!     EnsemblePolicy consumer (B-2 cap).
//!
//! ## Wire format
//!
//! `#[serde(tag = "type", rename_all = "snake_case")]` so a JSON
//! reader sees `{"type": "plan_issued", ...}` with the variant
//! payload flattened. Tag names are pinned by test so accidental
//! `#[serde]` attribute drift breaks the build, not consumers.

use serde::{Deserialize, Serialize};

use tars_types::AgentId;

use crate::orchestrator::Plan;

/// Typed inter-agent envelope. See module docs for which variant
/// flows in which direction.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    /// Orchestrator → Worker(s). Hands off the entire [`Plan`]; the
    /// Worker decides which step(s) it owns by inspecting
    /// `step.worker_role`. Future variants may carry a
    /// `WorkerAssignment` that pre-filters to one step, but for the
    /// first cut Workers see the whole plan so they can read prior
    /// steps' instructions for context.
    PlanIssued { plan: Plan },

    /// Worker → Orchestrator (or Aggregator).
    ///
    /// `step_id` is the [`crate::PlanStep::id`] this result addresses;
    /// `None` is allowed for free-standing Worker output that wasn't
    /// part of a plan (e.g. an ad-hoc Q&A worker).
    /// `confidence` is `0.0..=1.0` — `0.0` "no idea, asked the model
    /// and got nothing useful", `1.0` "I'm sure". Critic + Aggregator
    /// can weigh on this; values outside the range are clamped.
    PartialResult {
        from_agent: AgentId,
        step_id: Option<String>,
        summary: String,
        confidence: f32,
    },

    /// Critic → Orchestrator. The replanning trigger:
    /// `Approve` → loop continues; `Reject` → loop replans the
    /// failing step using the rejection reason; `Refine` → loop
    /// re-runs the step with the suggestion list as extra context.
    Verdict {
        from_agent: AgentId,
        target_step_id: Option<String>,
        verdict: VerdictKind,
    },

    /// Any agent → upstream. Surfaces "I need more info to make
    /// progress" instead of guessing. The orchestration loop's
    /// response is policy: forward to the user, fall back to a
    /// default, or treat as a Reject and replan.
    NeedsClarification {
        from_agent: AgentId,
        question: String,
    },
}

/// What a [`AgentMessage::Verdict`] decides. Fields per variant
/// carry the actionable detail the orchestration loop needs to
/// take the next step without re-parsing freeform text.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerdictKind {
    /// Critic accepted the work. No fields — `feedback` lives on
    /// the parent message's flows that may want commentary.
    Approve,
    /// Critic rejected the work. `reason` is human-readable; the
    /// orchestration loop uses it as input to the replan prompt.
    Reject { reason: String },
    /// Critic neither fully accepts nor fully rejects: try again
    /// with these specific changes. Suggestions are individual
    /// actionable items rather than one wall-of-text so the orchestration
    /// loop can prompt-construct cleanly.
    Refine { suggestions: Vec<String> },
}

impl AgentMessage {
    /// True iff this message is terminal for the Orchestrator's
    /// loop iteration — no further agents need to be invoked
    /// before the loop decides its next move. PlanIssued and
    /// PartialResult are intermediate; Verdict and
    /// NeedsClarification are terminal.
    pub fn is_terminal_for_orchestrator(&self) -> bool {
        matches!(self, Self::Verdict { .. } | Self::NeedsClarification { .. })
    }

    /// Best-effort one-line summary for the trajectory log
    /// (`AgentEvent::LlmCallCaptured::response_summary` etc.).
    /// Always returns something short; never panics.
    pub fn summary(&self) -> String {
        match self {
            Self::PlanIssued { plan } => {
                format!("plan_issued({} steps)", plan.steps.len())
            }
            Self::PartialResult {
                from_agent,
                step_id,
                confidence,
                ..
            } => {
                format!(
                    "partial_result(from={from_agent}, step={}, confidence={confidence:.2})",
                    step_id.as_deref().unwrap_or("-"),
                )
            }
            Self::Verdict {
                from_agent,
                target_step_id,
                verdict,
            } => {
                let kind = match verdict {
                    VerdictKind::Approve => "approve",
                    VerdictKind::Reject { .. } => "reject",
                    VerdictKind::Refine { .. } => "refine",
                };
                format!(
                    "verdict({kind} from={from_agent} target={})",
                    target_step_id.as_deref().unwrap_or("-"),
                )
            }
            Self::NeedsClarification { from_agent, .. } => {
                format!("needs_clarification(from={from_agent})")
            }
        }
    }
}

impl VerdictKind {
    /// True iff the orchestrator should continue with the current
    /// plan after seeing this verdict.
    pub fn approves(&self) -> bool {
        matches!(self, Self::Approve)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::orchestrator::PlanStep;

    fn sample_plan() -> Plan {
        Plan {
            plan_id: "p1".into(),
            goal: "summarise PR #42".into(),
            steps: vec![PlanStep {
                id: "s1".into(),
                worker_role: "summarise".into(),
                instruction: "do it".into(),
                depends_on: vec![],
            }],
        }
    }

    // ── Tag pinning ─────────────────────────────────────────────────
    //
    // These tests freeze the on-the-wire discriminator names. A
    // future #[serde] attribute change must update them deliberately.

    #[test]
    fn plan_issued_tag_is_snake_case() {
        let msg = AgentMessage::PlanIssued {
            plan: sample_plan(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], json!("plan_issued"));
        assert!(v["plan"].is_object());
    }

    #[test]
    fn partial_result_tag_is_snake_case() {
        let msg = AgentMessage::PartialResult {
            from_agent: AgentId::new("worker:summarise"),
            step_id: Some("s1".into()),
            summary: "done".into(),
            confidence: 0.8,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], json!("partial_result"));
    }

    #[test]
    fn verdict_kind_tag_is_snake_case() {
        let cases = [
            (VerdictKind::Approve, "approve"),
            (VerdictKind::Reject { reason: "x".into() }, "reject"),
            (
                VerdictKind::Refine {
                    suggestions: vec!["a".into(), "b".into()],
                },
                "refine",
            ),
        ];
        for (kind, expected_tag) in cases {
            let v = serde_json::to_value(&kind).unwrap();
            assert_eq!(v["kind"], json!(expected_tag));
        }
    }

    #[test]
    fn needs_clarification_tag_is_snake_case() {
        let msg = AgentMessage::NeedsClarification {
            from_agent: AgentId::new("worker"),
            question: "what now?".into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], json!("needs_clarification"));
    }

    // ── Round-trip ─────────────────────────────────────────────────

    #[test]
    fn every_variant_round_trips_through_json() {
        let cases: Vec<AgentMessage> = vec![
            AgentMessage::PlanIssued {
                plan: sample_plan(),
            },
            AgentMessage::PartialResult {
                from_agent: AgentId::new("a"),
                step_id: Some("s1".into()),
                summary: "ok".into(),
                confidence: 0.5,
            },
            AgentMessage::PartialResult {
                from_agent: AgentId::new("a"),
                step_id: None,
                summary: "ok".into(),
                confidence: 1.0,
            },
            AgentMessage::Verdict {
                from_agent: AgentId::new("critic"),
                target_step_id: Some("s1".into()),
                verdict: VerdictKind::Approve,
            },
            AgentMessage::Verdict {
                from_agent: AgentId::new("critic"),
                target_step_id: Some("s1".into()),
                verdict: VerdictKind::Reject {
                    reason: "too vague".into(),
                },
            },
            AgentMessage::Verdict {
                from_agent: AgentId::new("critic"),
                target_step_id: None,
                verdict: VerdictKind::Refine {
                    suggestions: vec!["add an example".into(), "shorten".into()],
                },
            },
            AgentMessage::NeedsClarification {
                from_agent: AgentId::new("worker"),
                question: "which branch?".into(),
            },
        ];
        for original in cases {
            let v = serde_json::to_value(&original).unwrap();
            let back: AgentMessage = serde_json::from_value(v.clone()).unwrap();
            // Re-serialize for comparison (AgentMessage isn't PartialEq).
            assert_eq!(serde_json::to_value(&back).unwrap(), v);
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────

    #[test]
    fn is_terminal_for_orchestrator_partitions_correctly() {
        assert!(
            !AgentMessage::PlanIssued {
                plan: sample_plan()
            }
            .is_terminal_for_orchestrator()
        );
        assert!(
            !AgentMessage::PartialResult {
                from_agent: AgentId::new("a"),
                step_id: None,
                summary: "x".into(),
                confidence: 1.0,
            }
            .is_terminal_for_orchestrator()
        );
        assert!(
            AgentMessage::Verdict {
                from_agent: AgentId::new("c"),
                target_step_id: None,
                verdict: VerdictKind::Approve,
            }
            .is_terminal_for_orchestrator()
        );
        assert!(
            AgentMessage::NeedsClarification {
                from_agent: AgentId::new("a"),
                question: "?".into(),
            }
            .is_terminal_for_orchestrator()
        );
    }

    #[test]
    fn verdict_approves_only_for_approve_variant() {
        assert!(VerdictKind::Approve.approves());
        assert!(!VerdictKind::Reject { reason: "x".into() }.approves());
        assert!(
            !VerdictKind::Refine {
                suggestions: vec![]
            }
            .approves()
        );
    }

    #[test]
    fn summary_is_short_and_non_panicking_for_every_variant() {
        let msgs: Vec<AgentMessage> = vec![
            AgentMessage::PlanIssued {
                plan: sample_plan(),
            },
            AgentMessage::PartialResult {
                from_agent: AgentId::new("a"),
                step_id: Some("s1".into()),
                summary: "x".repeat(10_000),
                confidence: 0.123,
            },
            AgentMessage::Verdict {
                from_agent: AgentId::new("c"),
                target_step_id: Some("s2".into()),
                verdict: VerdictKind::Refine {
                    suggestions: (0..100).map(|i| format!("suggestion {i}")).collect(),
                },
            },
            AgentMessage::NeedsClarification {
                from_agent: AgentId::new("w"),
                question: "x".repeat(10_000),
            },
        ];
        for m in msgs {
            let s = m.summary();
            assert!(s.len() < 200, "summary too long for {m:?}: {s}");
            assert!(!s.is_empty());
        }
    }

    // ── Cross-variant deserialization safety ───────────────────────
    //
    // The most likely consumer bug — a producer emits a variant the
    // consumer doesn't know about — is a JSON-parse failure. Pin
    // that behaviour so a future `#[serde(other)]` catchall isn't
    // added by mistake (we want loud failure, not silent skip).

    #[test]
    fn unknown_variant_fails_to_deserialize() {
        let v = json!({
            "type": "from_the_future",
            "made_up_field": "x",
        });
        let result: Result<AgentMessage, _> = serde_json::from_value(v);
        assert!(
            result.is_err(),
            "unknown variant must error, not silently drop"
        );
    }
}
