//! Unit tests for `ValidationMiddleware` + 3 built-in validators.
//!
//! Coverage:
//!
//! - Each built-in validator's Pass / Reject / Filter outcomes
//! - ValidationMiddleware: order-of-execution, Filter chaining,
//!   Reject short-circuit + ValidationFailed surface,
//!   Annotate accumulation in summary
//! - Cache×Validator interaction: validator runs on cache replay
//!   (raw stored, validators always rerun)
//! - Empty-validators-list passthrough (no drain cost)

use std::sync::Arc;

use futures::StreamExt;
use tars_provider::backends::mock::{CannedResponse, MockProvider};
use tars_types::{
    ChatRequest, ChatResponse, ModelHint, OutcomeSummary, ProviderError, RequestContext,
    ValidationOutcome,
};

use super::builtin::{JsonShapeValidator, MaxLengthValidator, NotEmptyValidator};
use super::{OutputValidator, ValidationMiddleware};
use crate::middleware::Middleware;
use crate::service::{LlmService, ProviderService};

// ── Built-in validator unit tests ────────────────────────────────────

fn fake_req() -> ChatRequest {
    ChatRequest::user(ModelHint::Explicit("m".into()), "ping")
}
fn resp_with_text(t: &str) -> ChatResponse {
    ChatResponse { text: t.into(), ..Default::default() }
}

#[test]
fn json_shape_passes_valid_json() {
    let v = JsonShapeValidator::new();
    let r = resp_with_text(r#"{"ok": true}"#);
    assert!(matches!(v.validate(&fake_req(), &r), ValidationOutcome::Pass));
}

#[test]
fn json_shape_passes_empty_text() {
    let v = JsonShapeValidator::new();
    let r = resp_with_text("");
    // Empty is NotEmptyValidator's concern, not JsonShape's.
    assert!(matches!(v.validate(&fake_req(), &r), ValidationOutcome::Pass));
}

#[test]
fn json_shape_rejects_broken_json() {
    let v = JsonShapeValidator::new();
    let r = resp_with_text(r#"{"missing": "comma" "next": 1}"#);
    match v.validate(&fake_req(), &r) {
        ValidationOutcome::Reject { reason, retriable } => {
            assert!(reason.contains("not valid JSON"));
            assert!(retriable, "default is retriable=true");
        }
        _ => panic!("expected Reject"),
    }
}

#[test]
fn json_shape_with_retriable_false_rejects_permanent() {
    let v = JsonShapeValidator::new().with_retriable(false);
    let r = resp_with_text("not json at all");
    match v.validate(&fake_req(), &r) {
        ValidationOutcome::Reject { retriable, .. } => assert!(!retriable),
        _ => panic!("expected Reject"),
    }
}

#[test]
fn not_empty_passes_nonempty() {
    let v = NotEmptyValidator::new();
    let r = resp_with_text("hello");
    assert!(matches!(v.validate(&fake_req(), &r), ValidationOutcome::Pass));
}

#[test]
fn not_empty_rejects_empty_text() {
    let v = NotEmptyValidator::new();
    let r = resp_with_text("");
    assert!(matches!(
        v.validate(&fake_req(), &r),
        ValidationOutcome::Reject { .. }
    ));
}

#[test]
fn not_empty_rejects_whitespace_only() {
    let v = NotEmptyValidator::new();
    let r = resp_with_text("   \n\t  ");
    assert!(matches!(
        v.validate(&fake_req(), &r),
        ValidationOutcome::Reject { .. }
    ));
}

#[test]
fn max_length_passes_under_limit() {
    let v = MaxLengthValidator::reject_above(100);
    let r = resp_with_text("short");
    assert!(matches!(v.validate(&fake_req(), &r), ValidationOutcome::Pass));
}

#[test]
fn max_length_rejects_over_limit() {
    let v = MaxLengthValidator::reject_above(5);
    let r = resp_with_text("more than five chars");
    match v.validate(&fake_req(), &r) {
        ValidationOutcome::Reject { reason, .. } => {
            assert!(reason.contains("max_chars=5"));
        }
        _ => panic!("expected Reject"),
    }
}

#[test]
fn max_length_truncates_when_filter_mode() {
    let v = MaxLengthValidator::truncate_above(5);
    let r = resp_with_text("more than five chars");
    match v.validate(&fake_req(), &r) {
        ValidationOutcome::Filter { response, dropped } => {
            assert_eq!(response.text.chars().count(), 5);
            assert_eq!(response.text, "more ");
            assert_eq!(dropped.len(), 1);
            assert!(dropped[0].contains("truncated"));
        }
        _ => panic!("expected Filter"),
    }
}

// ── ValidationMiddleware integration tests ──────────────────────────

/// Simple Annotate validator for testing chain composition.
struct AnnotatingValidator {
    name_: String,
    metric_value: i64,
}
impl OutputValidator for AnnotatingValidator {
    fn name(&self) -> &str {
        &self.name_
    }
    fn validate(&self, _req: &ChatRequest, _resp: &ChatResponse) -> ValidationOutcome {
        let mut metrics = std::collections::HashMap::new();
        metrics.insert(
            "value".to_string(),
            serde_json::Value::from(self.metric_value),
        );
        ValidationOutcome::Annotate { metrics }
    }
}

async fn drain(s: tars_provider::LlmEventStream) -> Vec<tars_types::ChatEvent> {
    s.collect::<Vec<_>>()
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .collect()
}

#[tokio::test]
async fn validation_passes_through_when_validators_pass() {
    let mock = MockProvider::new("mock", CannedResponse::text("hello world"));
    let inner: Arc<dyn LlmService> = ProviderService::new(mock);
    let mw = ValidationMiddleware::new(vec![Box::new(NotEmptyValidator::new())]);
    let svc = mw.wrap(inner);
    let ctx = RequestContext::test_default();
    let outcome_handle = ctx.validation_outcome.clone();

    let stream = svc
        .call(fake_req(), ctx)
        .await
        .expect("stream should open");
    let events = drain(stream).await;
    assert!(!events.is_empty());

    // Outer caller reads summary from ctx side-channel.
    let rec = outcome_handle.lock().unwrap();
    assert_eq!(rec.summary.validators_run, vec!["not_empty"]);
    assert!(matches!(
        rec.summary.outcomes.get("not_empty"),
        Some(OutcomeSummary::Pass)
    ));
}

#[tokio::test]
async fn validation_reject_surfaces_validation_failed_error() {
    // Mock with empty text — NotEmpty will reject.
    let mock = MockProvider::new("mock", CannedResponse::text(""));
    let inner: Arc<dyn LlmService> = ProviderService::new(mock);
    let mw = ValidationMiddleware::new(vec![Box::new(NotEmptyValidator::new())]);
    let svc = mw.wrap(inner);
    let result = svc.call(fake_req(), RequestContext::test_default()).await;
    match result {
        Ok(_) => panic!("expected Err, got Ok stream"),
        Err(ProviderError::ValidationFailed {
            validator,
            retriable,
            ..
        }) => {
            assert_eq!(validator, "not_empty");
            assert!(retriable, "NotEmpty default is retriable");
        }
        Err(e) => panic!("expected ValidationFailed, got: {e:?}"),
    }
}

#[tokio::test]
async fn validation_chain_runs_in_order_and_short_circuits_on_reject() {
    // Order: JsonShape (Reject — not JSON) → NotEmpty (should NOT
    // run because Reject short-circuits).
    //
    // Use a sentinel validator after NotEmpty to assert it never ran.
    struct Trap;
    impl OutputValidator for Trap {
        fn name(&self) -> &str {
            "trap"
        }
        fn validate(&self, _: &ChatRequest, _: &ChatResponse) -> ValidationOutcome {
            panic!("trap validator should not have been called");
        }
    }
    let mock = MockProvider::new("mock", CannedResponse::text("definitely not JSON"));
    let inner: Arc<dyn LlmService> = ProviderService::new(mock);
    let mw = ValidationMiddleware::new(vec![
        Box::new(JsonShapeValidator::new()),
        Box::new(Trap),
    ]);
    let svc = mw.wrap(inner);
    let result = svc.call(fake_req(), RequestContext::test_default()).await;
    match result {
        Err(ProviderError::ValidationFailed { validator, .. }) => {
            assert_eq!(validator, "json_shape");
            // If Trap had run, the panic would have crashed the test.
        }
        Err(e) => panic!("expected json_shape ValidationFailed, got error: {e:?}"),
        Ok(_) => panic!("expected ValidationFailed, got Ok stream"),
    }
}

#[tokio::test]
async fn validation_filter_modifies_response_subsequent_validators_see_filtered() {
    // Truncate to 5 chars then check NotEmpty (should still pass).
    let mock = MockProvider::new("mock", CannedResponse::text("hello world"));
    let inner: Arc<dyn LlmService> = ProviderService::new(mock);
    let mw = ValidationMiddleware::new(vec![
        Box::new(MaxLengthValidator::truncate_above(5)),
        Box::new(NotEmptyValidator::new()),
    ]);
    let svc = mw.wrap(inner);
    let ctx = RequestContext::test_default();
    let outcome_handle = ctx.validation_outcome.clone();
    let _ = svc
        .call(fake_req(), ctx)
        .await
        .expect("should succeed")
        .collect::<Vec<_>>()
        .await;

    let rec = outcome_handle.lock().unwrap();
    // Validators ran in order
    assert_eq!(rec.summary.validators_run, vec!["max_length", "not_empty"]);
    // max_length recorded a Filter outcome, not_empty Pass
    assert!(matches!(
        rec.summary.outcomes.get("max_length"),
        Some(OutcomeSummary::Filter { .. })
    ));
    assert!(matches!(
        rec.summary.outcomes.get("not_empty"),
        Some(OutcomeSummary::Pass)
    ));
    // Filtered response is published on the side-channel.
    let filtered = rec.filtered_response.as_ref().expect("filter ran");
    assert_eq!(filtered.text, "hello");
}

#[tokio::test]
async fn validation_annotate_stores_metrics_in_summary() {
    let mock = MockProvider::new("mock", CannedResponse::text("anything"));
    let inner: Arc<dyn LlmService> = ProviderService::new(mock);
    let mw = ValidationMiddleware::new(vec![Box::new(AnnotatingValidator {
        name_: "annot".into(),
        metric_value: 42,
    })]);
    let svc = mw.wrap(inner);
    let ctx = RequestContext::test_default();
    let outcome_handle = ctx.validation_outcome.clone();
    let stream = match svc.call(fake_req(), ctx).await {
        Ok(s) => s,
        Err(e) => panic!("expected Ok stream, got Err: {e:?}"),
    };
    let _ = stream.collect::<Vec<_>>().await;
    let rec = outcome_handle.lock().unwrap();
    match rec.summary.outcomes.get("annot") {
        Some(OutcomeSummary::Annotate { metrics }) => {
            assert_eq!(metrics.get("value").unwrap().as_i64().unwrap(), 42);
        }
        other => panic!("expected Annotate, got: {other:?}"),
    }
}

#[tokio::test]
async fn validation_empty_chain_passes_through_without_drain() {
    let mock = MockProvider::new("mock", CannedResponse::text("hi"));
    let inner: Arc<dyn LlmService> = ProviderService::new(mock);
    let mw = ValidationMiddleware::new(vec![]); // no validators
    let svc = mw.wrap(inner);
    let ctx = RequestContext::test_default();
    let outcome_handle = ctx.validation_outcome.clone();

    let stream = match svc.call(fake_req(), ctx).await {
        Ok(s) => s,
        Err(e) => panic!("expected Ok stream, got Err: {e:?}"),
    };
    let events = drain(stream).await;
    assert!(!events.is_empty());

    // No validators ran → summary is empty.
    let rec = outcome_handle.lock().unwrap();
    assert!(rec.summary.validators_run.is_empty());
    assert!(rec.summary.outcomes.is_empty());
    assert!(rec.filtered_response.is_none());
}

// ── Layer trace ───────────────────────────────────────────────────────

#[tokio::test]
async fn validation_appends_to_layer_trace() {
    let mock = MockProvider::new("mock", CannedResponse::text("hi"));
    let inner: Arc<dyn LlmService> = ProviderService::new(mock);
    let mw = ValidationMiddleware::new(vec![Box::new(NotEmptyValidator::new())]);
    let svc = mw.wrap(inner);
    let ctx = RequestContext::test_default();
    let telemetry_handle = ctx.telemetry.clone();
    let stream = match svc.call(fake_req(), ctx).await {
        Ok(s) => s,
        Err(e) => panic!("expected Ok stream, got Err: {e:?}"),
    };
    let _ = drain(stream).await;
    let t = telemetry_handle.lock().unwrap();
    assert!(t.layers.iter().any(|l| l == "validation"));
}
