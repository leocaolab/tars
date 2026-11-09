use std::sync::Arc;
use tars_types::ProviderId;

use crate::LlmError;
use crate::provider::backends::mock::{CannedResponse, MockProvider};
use futures::StreamExt;
use tars_types::{
    Caller, ChatRequest, ChatResponse, OutcomeSummary, ValidationOutcome, ValidationReason,
};

use super::builtin::{JsonShapeValidator, MaxLengthValidator, NotEmptyValidator};
use super::{OutputValidator, ValidationMiddleware};
use crate::pipeline::service::LlmService;

// ── Built-in validator unit tests ────────────────────────────────────

fn fake_req() -> ChatRequest {
    ChatRequest::user("ping")
}
fn resp_with_text(t: &str) -> ChatResponse {
    ChatResponse {
        actual_model: None,
        text: t.into(),
        thinking: String::new(),
        tool_calls: Vec::new(),
        stop_reason: tars_types::StopReason::EndTurn,
        usage: None,
        cache_hit: Default::default(),
        degraded: Vec::new(),
        validation_summary: Default::default(),
        created: 0,
    }
}

#[test]
fn json_shape_passes_valid_json() {
    let v = JsonShapeValidator::new();
    let r = resp_with_text(r#"{"ok": true}"#);
    assert!(matches!(
        v.validate(&fake_req(), &r),
        ValidationOutcome::Pass
    ));
}

#[test]
fn json_shape_passes_empty_text() {
    let v = JsonShapeValidator::new();
    let r = resp_with_text("");
    // Empty is NotEmptyValidator's concern, not JsonShape's.
    assert!(matches!(
        v.validate(&fake_req(), &r),
        ValidationOutcome::Pass
    ));
}

#[test]
fn json_shape_rejects_broken_json() {
    let v = JsonShapeValidator::new();
    let r = resp_with_text(r#"{"missing": "comma" "next": 1}"#);
    match v.validate(&fake_req(), &r) {
        ValidationOutcome::Reject { reason } => {
            assert!(matches!(reason, ValidationReason::JsonShape { .. }));
            assert_eq!(reason.kind(), "json_shape");
            assert!(reason.to_string().contains("not valid JSON"));
        }
        _ => panic!("expected Reject"),
    }
}

#[test]
fn not_empty_passes_nonempty() {
    let v = NotEmptyValidator::new();
    let r = resp_with_text("hello");
    assert!(matches!(
        v.validate(&fake_req(), &r),
        ValidationOutcome::Pass
    ));
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
    assert!(matches!(
        v.validate(&fake_req(), &r),
        ValidationOutcome::Pass
    ));
}

#[test]
fn max_length_rejects_over_limit() {
    let v = MaxLengthValidator::reject_above(5);
    let r = resp_with_text("more than five chars");
    match v.validate(&fake_req(), &r) {
        ValidationOutcome::Reject { reason } => {
            assert_eq!(reason.kind(), "max_length");
            assert!(
                matches!(reason, ValidationReason::MaxLength { max: 5, .. }),
                "expected typed MaxLength{{max:5}}, got {reason:?}"
            );
            assert!(reason.to_string().contains("max_chars=5"));
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

async fn drain(s: crate::LlmEventStream) -> Vec<tars_types::ChatEvent> {
    // Surface provider errors rather than silently dropping them with
    // `filter_map(.ok())` — a swallowed Err would mask a regression where
    // validation turns a clean stream into an errored one.
    s.collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|r| r.expect("drain: stream yielded a provider error"))
        .collect()
}

/// Returns the response a caller builds from `events`, the validation summary included.
fn built(events: Vec<tars_types::ChatEvent>) -> ChatResponse {
    let mut b = tars_types::ChatResponseBuilder::new();
    events.into_iter().for_each(|ev| b.apply(ev).unwrap());
    b.finish().unwrap()
}

#[tokio::test]
async fn validation_passes_through_when_validators_pass() {
    let mock = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("hello world"));
    let mw = ValidationMiddleware::new(vec![
        Arc::new(NotEmptyValidator::new()) as Arc<dyn OutputValidator>
    ]);
    let svc = LlmService::builder(mock, "test-model").layer(mw).build();
    let ctx = Caller::test_default();

    let stream = svc
        .call(fake_req(), ctx)
        .await
        .result
        .expect("stream should open");
    let events = drain(stream).await;
    assert!(!events.is_empty());

    // The outer caller reads the summary from the stream.
    let summary = built(events).validation_summary;
    assert_eq!(summary.validators_run, vec!["not_empty"]);
    assert!(matches!(
        summary.outcomes.get("not_empty"),
        Some(OutcomeSummary::Pass)
    ));
}

#[tokio::test]
async fn validation_reject_surfaces_validation_failed_error() {
    // Mock with empty text — NotEmpty will reject.
    let mock = MockProvider::new(ProviderId::new("mock"), CannedResponse::text(""));
    let mw = ValidationMiddleware::new(vec![
        Arc::new(NotEmptyValidator::new()) as Arc<dyn OutputValidator>
    ]);
    let svc = LlmService::builder(mock, "test-model").layer(mw).build();
    let result = svc.call(fake_req(), Caller::test_default()).await.result;
    match result {
        Ok(_) => panic!("expected Err, got Ok stream"),
        Err(LlmError::ValidationFailed { validator, .. }) => {
            assert_eq!(validator, "not_empty");
            // ValidationFailed is always Permanent.
            let err = LlmError::ValidationFailed {
                validator: "not_empty".into(),
                reason: Box::new(ValidationReason::NotEmpty {
                    field: "text".into(),
                }),
                raw: String::new(),
            };
            assert_eq!(err.class(), tars_types::ErrorClass::Permanent);
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
    let mock = MockProvider::new(
        ProviderId::new("mock"),
        CannedResponse::text("definitely not JSON"),
    );
    let mw = ValidationMiddleware::new(vec![
        Arc::new(JsonShapeValidator::new()) as Arc<dyn OutputValidator>,
        Arc::new(Trap),
    ]);
    let svc = LlmService::builder(mock, "test-model").layer(mw).build();
    let result = svc.call(fake_req(), Caller::test_default()).await.result;
    match result {
        Err(LlmError::ValidationFailed { validator, .. }) => {
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
    let mock = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("hello world"));
    let mw = ValidationMiddleware::new(vec![
        Arc::new(MaxLengthValidator::truncate_above(5)) as Arc<dyn OutputValidator>,
        Arc::new(NotEmptyValidator::new()),
    ]);
    let svc = LlmService::builder(mock, "test-model").layer(mw).build();
    let ctx = Caller::test_default();
    let collected: Vec<_> = svc
        .call(fake_req(), ctx)
        .await
        .result
        .expect("should succeed")
        .collect()
        .await;
    assert!(
        collected.iter().all(Result::is_ok),
        "stream must not yield any provider errors",
    );

    let filtered = built(collected.into_iter().map(Result::unwrap).collect());
    let summary = &filtered.validation_summary;
    assert_eq!(summary.validators_run, vec!["max_length", "not_empty"]);
    assert!(matches!(
        summary.outcomes.get("max_length"),
        Some(OutcomeSummary::Filter { .. })
    ));
    assert!(matches!(
        summary.outcomes.get("not_empty"),
        Some(OutcomeSummary::Pass)
    ));
    // The filtered response is what the stream carries.
    assert_eq!(filtered.text, "hello");
}

#[tokio::test]
async fn validation_annotate_stores_metrics_in_summary() {
    let mock = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("anything"));
    let mw = ValidationMiddleware::new(vec![Arc::new(AnnotatingValidator {
        name_: "annot".into(),
        metric_value: 42,
    }) as Arc<dyn OutputValidator>]);
    let svc = LlmService::builder(mock, "test-model").layer(mw).build();
    let ctx = Caller::test_default();
    let stream = match svc.call(fake_req(), ctx).await.result {
        Ok(s) => s,
        Err(e) => panic!("expected Ok stream, got Err: {e:?}"),
    };
    let collected: Vec<_> = stream.collect().await;
    assert!(
        collected.iter().all(Result::is_ok),
        "stream must not yield any provider errors",
    );
    let summary = built(collected.into_iter().map(Result::unwrap).collect()).validation_summary;
    match summary.outcomes.get("annot") {
        Some(OutcomeSummary::Annotate { metrics }) => {
            assert_eq!(metrics.get("value").unwrap().as_i64().unwrap(), 42);
        }
        other => panic!("expected Annotate, got: {other:?}"),
    }
}

#[tokio::test]
async fn validation_empty_chain_passes_through_without_drain() {
    let mock = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("hi"));
    let mw = ValidationMiddleware::new(Vec::<Arc<dyn OutputValidator>>::new()); // no validators
    let svc = LlmService::builder(mock, "test-model").layer(mw).build();
    let ctx = Caller::test_default();

    let stream = match svc.call(fake_req(), ctx).await.result {
        Ok(s) => s,
        Err(e) => panic!("expected Ok stream, got Err: {e:?}"),
    };
    let events = drain(stream).await;
    assert!(!events.is_empty());

    let published = events
        .iter()
        .any(|ev| matches!(ev, tars_types::ChatEvent::Validated { .. }));
    let summary = built(events).validation_summary;
    assert!(summary.validators_run.is_empty());
    assert!(summary.outcomes.is_empty());
    assert!(!published);
}

// ── Layer trace ───────────────────────────────────────────────────────

#[tokio::test]
async fn validation_appends_to_layer_trace() {
    let mock = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("hi"));
    let mw = ValidationMiddleware::new(vec![
        Arc::new(NotEmptyValidator::new()) as Arc<dyn OutputValidator>
    ]);
    let svc = LlmService::builder(mock, "test-model").layer(mw).build();
    let ctx = Caller::test_default();
    let stream = match svc.call(fake_req(), ctx).await.result {
        Ok(s) => s,
        Err(e) => panic!("expected Ok stream, got Err: {e:?}"),
    };
    let _ = drain(stream).await;
    assert!(svc.layer_names().contains(&"validation"));
}

// ── The chain does not run on the async thread ──────────────────────

/// How long the blocking validator waits for the async task before it gives up — a
/// failure bound only: on a free runtime the reply comes back in microseconds.
const ASYNC_REPLY_CAP: std::time::Duration = std::time::Duration::from_secs(10);

/// A validator that blocks its thread until an async task on the same runtime answers —
/// the shape of a Python validator waiting for (or holding) the GIL. It tells the task it
/// has started, so the task cannot have answered before the validator was blocking.
struct WaitsForTheRuntime {
    started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    reply: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl OutputValidator for WaitsForTheRuntime {
    fn name(&self) -> &str {
        "waits_for_the_runtime"
    }
    fn validate(&self, _req: &ChatRequest, _resp: &ChatResponse) -> ValidationOutcome {
        if let Some(started) = self.started.lock().unwrap().take() {
            started.send(()).unwrap();
        }
        match self.reply.lock().unwrap().recv_timeout(ASYNC_REPLY_CAP) {
            Ok(()) => ValidationOutcome::Pass,
            Err(e) => ValidationOutcome::Reject {
                reason: ValidationReason::Custom {
                    kind: "frozen".into(),
                    message: format!("the async task never ran while this validator blocked: {e}"),
                    detail: None,
                },
            },
        }
    }
}

// `#[tokio::test]` is a current-thread runtime: one thread polls every task, so a
// validator run inline on it freezes the task it is waiting for — the one-worker form of
// a runtime whose every worker is inside a blocking validator.
#[tokio::test]
async fn a_blocking_validator_does_not_freeze_other_tasks_on_the_runtime() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let unrelated = tokio::spawn(async move {
        started_rx.await.unwrap();
        reply_tx.send(()).unwrap();
    });
    let validator = WaitsForTheRuntime {
        started: std::sync::Mutex::new(Some(started_tx)),
        reply: std::sync::Mutex::new(reply_rx),
    };
    let mock = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("hi"));
    let mw = ValidationMiddleware::new(vec![Arc::new(validator) as Arc<dyn OutputValidator>]);
    let svc = LlmService::builder(mock, "test-model").layer(mw).build();

    let result = svc.call(fake_req(), Caller::test_default()).await.result;

    if let Err(e) = result {
        panic!("the validator blocked the runtime's only thread: {e:?}");
    }
    unrelated.await.unwrap();
}
