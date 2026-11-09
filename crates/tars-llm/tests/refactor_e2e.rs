//! Checks contracts seen from outside the crate: `RetryAttempt`'s wire form and
//! `PerCallBudgetMiddleware::new` on valid pricing.

use std::collections::HashSet;
use tars_types::ProviderId;

use tars_llm::PerCallBudgetMiddleware;
use tars_llm::provider::backends::mock::{CannedResponse, MockProvider};
use tars_types::{LlmErrorKind, Pricing, RetryAttempt};

#[test]
fn retry_attempt_construction_carries_typed_kind() {
    // Readers of the call log (Python included) read the kind as a snake_case string.
    let attempt = RetryAttempt {
        error_kind: LlmErrorKind::Network,
        retry_after_ms: Some(500),
    };
    let json = serde_json::to_string(&attempt).unwrap();
    assert!(json.contains(r#""error_kind":"network""#), "got: {json}");

    let back: RetryAttempt = serde_json::from_str(&json).unwrap();
    assert_eq!(back.error_kind, LlmErrorKind::Network);
}

#[test]
fn try_new_with_valid_capabilities_round_trips_through_wrap() {
    use tars_types::ProviderProfile;
    let caps = ProviderProfile::text_only_baseline(Pricing {
        input_per_million: tars_types::Rate::new(3.0).unwrap(),
        output_per_million: tars_types::Rate::new(15.0).unwrap(),
        ..Pricing::default()
    });
    let mw = PerCallBudgetMiddleware::new(1.0, &caps).expect("valid caps must construct");
    let mock = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hi"));
    let inner: tars_llm::LlmService = tars_llm::LlmService::of(mock, "test-model");
    let _wrapped = tars_llm::LlmService::builder_with_inner(inner)
        .layer(mw)
        .build();
}

#[test]
fn provider_error_kind_set_contains_uses_typed_equality() {
    let mut kinds: HashSet<LlmErrorKind> = HashSet::new();
    kinds.insert(LlmErrorKind::Network);
    kinds.insert(LlmErrorKind::Unavailable);

    assert!(kinds.contains(&LlmErrorKind::Network));
    assert!(!kinds.contains(&LlmErrorKind::Auth));
}
