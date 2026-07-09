//! Pipeline-level event types — one event per `Pipeline.call` boundary.
//! See [Doc 17](../../../docs/architecture/17-pipeline-event-store.md).
//!
//! Distinct from `crate::events::ChatEvent` (streaming-token contract,
//! per-token granularity) and from `tars-runtime`'s `AgentEvent`
//! (agent-decision granularity). This stream sits at the LLM-call
//! boundary: one entry per completed `Pipeline.call`, regardless of
//! whether a Session wraps it.
//!
//! Schema lives in `tars-types` (data contract, no backend); the
//! `PipelineEventLog` trait that persists it lives in `tars_melt::event`.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::content_ref::ContentRef;
use crate::error::ProviderErrorKind;
use crate::events::StopReason;
use crate::ids::{ProviderId, SessionId, TenantId, TraceId};
use crate::telemetry::TelemetryAccumulator;
use crate::usage::Usage;
use crate::validation::{ValidationReason, ValidationSummary};

/// Top-level event variant. `#[non_exhaustive]` plus a catchall
/// `Other` variant give two layers of forward-compat: old readers
/// don't fail on unknown variants, and new variants can be added
/// without SemVer break (forward-compat catchall pattern; see
/// Doc 17 §4).
///
/// Variants box their inner structs (`LlmCallFinished` is ~600 bytes;
/// boxing keeps the enum's stack footprint to a pointer) so consumers
/// holding `Vec<PipelineEvent>` don't pay 600 bytes per slot when
/// most slots will end up `LlmCallFinished` anyway.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PipelineEvent {
    /// One completed `Pipeline.call` — success or failure.
    LlmCallFinished(Box<LlmCallFinished>),

    /// Score produced by an evaluator (Online or Offline). FK back
    /// to the `LlmCallFinished` it scored. Defined now; not yet
    /// emitted — Phase 2 / W3 main body wires the runner that
    /// generates these.
    EvaluationScored(Box<EvaluationScored>),

    /// Forward-compat catchall. Old readers deserialise unknown
    /// variants here instead of failing the whole record. Caller
    /// code shouldn't construct `Other` directly — the variant
    /// exists so that `serde_json::from_value` succeeds on payloads
    /// from a future schema version.
    #[serde(other)]
    Other,
}

/// Per-call observability + outcome record. Inline scalars (small,
/// queryable) plus `ContentRef` pointers to records in a separate
/// `LlmRecordStore` (Doc 17 §5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmCallFinished {
    // ── identity ────────────────────────────────────────────────
    pub event_id: Uuid,
    pub timestamp: SystemTime,
    pub tenant_id: TenantId,
    /// `None` when the caller invoked `Pipeline.complete` directly
    /// without a `Session`.
    pub session_id: Option<SessionId>,
    /// For B-21 OTel cross-system correlation. May be `None` if the
    /// caller didn't supply one; tars never invents trace IDs.
    pub trace_id: Option<TraceId>,

    // ── request (inline scalars + body ref) ─────────────────────
    /// Provider that actually ran the call. `None` when routing
    /// short-circuited before provider resolution (cache hit, early
    /// validation failure) — historically the same state was carried
    /// as the `ProviderId::new("unresolved")` sentinel string (fix
    /// ARC-L5-SW-10); old events with that literal value are
    /// rewritten to `None` by the tars-storage v1→v2 schema
    /// migration.
    pub provider_id: Option<ProviderId>,
    /// Model actually called — post-routing resolution, not the
    /// caller's `ModelHint`. Useful for "which model did each
    /// candidate routing actually pick" queries.
    pub actual_model: String,
    /// sha256 of the canonical request body, **tenant-agnostic** —
    /// used for cross-tenant analytics (`"this prompt template
    /// appeared 10000 times across tenants"`). Distinct from
    /// `request_ref` (which is tenant-scoped) and from cache key
    /// (which adds tenant + IAM scopes).
    pub request_fingerprint: [u8; 32],
    /// Pointer to the full request record in `LlmRecordStore`. Tenant-scoped.
    pub request_ref: ContentRef,
    pub has_tools: bool,
    pub has_thinking: bool,
    pub has_structured_output: bool,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,

    // ── response ────────────────────────────────────────────────
    /// `None` when the call failed before producing a response.
    pub response_ref: Option<ContentRef>,
    pub usage: Usage,
    pub stop_reason: Option<StopReason>,

    // ── observability snapshots ─────────────────────────────────
    /// In-memory accumulator captured at end-of-call. Same shape as
    /// what `Response.telemetry` exposes today (B-15 / Stage 4).
    pub telemetry: TelemetryAccumulator,
    /// Per-validator outcomes captured at end-of-call. Empty when
    /// no validators ran.
    pub validation_summary: ValidationSummary,
    /// Why a validator **rejected** this call, when one did. `Some`
    /// only on the `ValidationFailed` path; `None` otherwise.
    ///
    /// Symmetric with [`Self::validation_summary`]: that field records
    /// the Pass / Filter / Annotate outcomes of a call that produced a
    /// Response, but a Reject short-circuits *before* a Response exists,
    /// so it never lands in the summary. This field is the reject's
    /// home — together they give the complete validator picture, and an
    /// evaluator can facet on the structured reason (`reason.kind()` +
    /// detail) rather than re-deriving it from the bare
    /// `result: error{kind: validation_failed}` discriminant.
    ///
    /// `#[serde(default, skip_serializing_if)]` keeps the persisted JSON
    /// byte-identical for the overwhelming majority of events (every
    /// non-validation-reject call), so this is a backward-compatible
    /// schema addition: older event blobs without the field deserialize
    /// to `None`. (B-20.v2 follow-up.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_reason: Option<ValidationReason>,

    // ── outcome ─────────────────────────────────────────────────
    pub result: CallResult,

    // ── cohort ──────────────────────────────────────────────────
    /// Free-form tags from `RequestContext::with_tags(...)` /
    /// `Session::tagged(...)`. Cohort SQL: `WHERE 'dogfood_X' =
    /// ANY(tags)`.
    pub tags: Vec<String>,
}

/// Top-level outcome of one call. Fine-grained error info stays in
/// the per-attempt log + `telemetry.retry_attempts`; this field is
/// just enough for "did it work" rollups.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CallResult {
    Ok,
    Error {
        /// Typed error discriminator — the same [`ProviderErrorKind`]
        /// every other consumer (telemetry's `error_kind`,
        /// `TarsProviderError.kind`) is keyed on. Serializes to the
        /// identical snake_case wire string ("rate_limited", "network",
        /// "validation_failed", ...) it used to carry as a raw `String`,
        /// so the persisted JSON form is unchanged.
        kind: ProviderErrorKind,
    },
}

/// Score produced by an evaluator. Defined for forward schema
/// compat; not yet emitted — Phase 2 wires the runner.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationScored {
    pub event_id: Uuid,
    pub timestamp: SystemTime,
    pub tenant_id: TenantId,
    /// FK to the `LlmCallFinished.event_id` this score evaluates.
    pub call_event_id: Uuid,
    pub evaluator_name: String,
    pub score: f64,
    /// Optional explanation (LLM-as-judge rationale, deterministic
    /// evaluator's failed-rule list, ...).
    pub explanation: Option<String>,
    pub tags: Vec<String>,
}

/// Per-tenant persistence dial. Default `Limited`. See Doc 17 §8.1.
///
/// Distinct from sampling — sampling decides "do we emit this call
/// at all", `PersistenceMode` decides "if we emit, how much detail
/// goes in." Both compose.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum PersistenceMode {
    /// Default — inline scalars + ContentRef records. Sufficient for
    /// metric rollups, cohort filtering, regression gates.
    #[default]
    Limited,

    /// Extended debug detail — per-attempt retry payloads, raw
    /// stream timing, intermediate tool-call args/results.
    /// ~5-10× storage cost vs Limited. Tenant opts in for debugging
    /// windows.
    Extended,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_finished() -> LlmCallFinished {
        LlmCallFinished {
            event_id: Uuid::new_v4(),
            timestamp: SystemTime::now(),
            tenant_id: TenantId::new("t"),
            session_id: None,
            trace_id: None,
            provider_id: Some(ProviderId::new("p")),
            actual_model: "m".into(),
            request_fingerprint: [0u8; 32],
            request_ref: ContentRef::from_content(TenantId::new("t"), b"req"),
            has_tools: false,
            has_thinking: false,
            has_structured_output: false,
            temperature: Some(0.0),
            max_output_tokens: None,
            response_ref: None,
            usage: Usage::default(),
            stop_reason: None,
            telemetry: TelemetryAccumulator::default(),
            validation_summary: ValidationSummary::default(),
            validation_reason: None,
            result: CallResult::Ok,
            tags: vec![],
        }
    }

    #[test]
    fn roundtrip_llm_call_finished() {
        let ev = PipelineEvent::LlmCallFinished(Box::new(fake_finished()));
        let json = serde_json::to_value(&ev).expect("ser");
        let back: PipelineEvent = serde_json::from_value(json).expect("de");
        match back {
            PipelineEvent::LlmCallFinished(_) => {}
            other => panic!("expected LlmCallFinished, got {other:?}"),
        }
    }

    #[test]
    fn unknown_variant_deserialises_into_other() {
        // Simulate a future schema version emitting a variant we don't
        // know yet. Old readers must accept this, not panic.
        let payload = serde_json::json!({
            "type": "future_event_type_we_dont_know",
            "some_field": 42,
        });
        let parsed: PipelineEvent = serde_json::from_value(payload).expect("de");
        assert!(matches!(parsed, PipelineEvent::Other));
    }

    #[test]
    fn persistence_mode_defaults_to_limited() {
        assert_eq!(PersistenceMode::default(), PersistenceMode::Limited);
    }

    #[test]
    fn call_result_serialises_with_kind_field() {
        let r = CallResult::Error {
            kind: ProviderErrorKind::RateLimited,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["result"], "error");
        assert_eq!(v["kind"], "rate_limited");
    }

    #[test]
    fn validation_reason_omitted_from_wire_when_none() {
        // Backward-compat: the overwhelming majority of events carry no
        // reject reason; the field must not appear in their JSON so the
        // persisted form is byte-identical to pre-B-20.v2 blobs.
        let ev = fake_finished(); // validation_reason: None
        let v = serde_json::to_value(&ev).unwrap();
        assert!(
            v.get("validation_reason").is_none(),
            "None reason must be skipped on the wire, got {v}"
        );
    }

    #[test]
    fn old_event_without_validation_reason_deserialises_to_none() {
        // An event blob persisted before the field existed must still
        // deserialize (field is `#[serde(default)]`).
        let mut v = serde_json::to_value(fake_finished()).unwrap();
        v.as_object_mut().unwrap().remove("validation_reason");
        let back: LlmCallFinished = serde_json::from_value(v).expect("de without field");
        assert!(back.validation_reason.is_none());
    }

    #[test]
    fn validation_reason_round_trips_on_event() {
        let mut ev = fake_finished();
        ev.validation_reason = Some(crate::ValidationReason::NotEmpty {
            field: "text".into(),
        });
        ev.result = CallResult::Error {
            kind: ProviderErrorKind::ValidationFailed,
        };
        let v = serde_json::to_value(&ev).unwrap();
        let back: LlmCallFinished = serde_json::from_value(v).expect("de");
        assert_eq!(back.validation_reason, ev.validation_reason);
    }
}
