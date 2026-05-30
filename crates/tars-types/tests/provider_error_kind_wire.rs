//! Integration tests for [`tars_types::ProviderErrorKind`]'s wire
//! contract. The whole point of typing `kind: ProviderErrorKind`
//! instead of `kind: String` is that the persisted JSON form on event
//! stores / Python interop / `RetryAttempt` rows stays
//! byte-for-byte identical — these tests pin that invariant down so a
//! future careless rename of an enum variant trips a loud test
//! failure instead of silently breaking downstream consumers.

use tars_types::{ProviderError, ProviderErrorKind, RetryAttempt};

/// The full set of `(ProviderErrorKind, wire_string)` pairs we
/// promise to consumers. Updating either side without the other is a
/// breaking change.
fn expected_kind_wire_pairs() -> Vec<(ProviderErrorKind, &'static str)> {
    use ProviderErrorKind as K;
    vec![
        (K::Auth, "auth"),
        (K::RateLimited, "rate_limited"),
        (K::BudgetExceeded, "budget_exceeded"),
        (K::InvalidRequest, "invalid_request"),
        (K::ContentFiltered, "content_filtered"),
        (K::ContextTooLong, "context_too_long"),
        (K::ModelOverloaded, "model_overloaded"),
        (K::CircuitOpen, "circuit_open"),
        (K::Network, "network"),
        (K::Parse, "parse"),
        (K::CliSubprocessDied, "cli_subprocess_died"),
        (K::UnknownTool, "unknown_tool"),
        (K::NoCompatibleCandidate, "no_compatible_candidate"),
        (K::ValidationFailed, "validation_failed"),
        (K::Internal, "internal"),
    ]
}

#[test]
fn as_str_matches_serde_serialised_form_for_every_variant() {
    // The serde annotation (`rename_all = "snake_case"`) and the
    // hand-written `as_str()` must agree variant-for-variant.
    for (kind, wire) in expected_kind_wire_pairs() {
        let serde_str = serde_json::to_string(&kind).unwrap();
        let unquoted = serde_str.trim_matches('"');
        assert_eq!(
            kind.as_str(),
            unquoted,
            "as_str() and serde disagree for {kind:?}: as_str={:?} serde={:?}",
            kind.as_str(),
            unquoted,
        );
        assert_eq!(kind.as_str(), wire, "as_str drift for {kind:?}");
    }
}

#[test]
fn deserialise_round_trips_for_every_variant() {
    for (kind, _wire) in expected_kind_wire_pairs() {
        let s = serde_json::to_string(&kind).unwrap();
        let parsed: ProviderErrorKind = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, kind, "round-trip failed for {kind:?} via {s}");
    }
}

#[test]
fn deserialise_rejects_unknown_variant() {
    let err = serde_json::from_str::<ProviderErrorKind>(r#""not_a_real_kind""#).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not_a_real_kind") || msg.contains("unknown variant"),
        "unexpected serde error: {msg}"
    );
}

#[test]
fn retry_attempt_wire_form_unchanged_from_pre_typing() {
    // Persisted retry-attempt rows from before the
    // String → ProviderErrorKind switch must still deserialise. The
    // legacy form is exactly what `String` produced through
    // `RetryAttempt`'s `Serialize` derive, so we hand-roll the JSON
    // here as the pinned wire contract.
    let legacy_json = r#"{"error_kind":"rate_limited","retry_after_ms":250}"#;
    let parsed: RetryAttempt = serde_json::from_str(legacy_json).unwrap();
    assert_eq!(parsed.error_kind, ProviderErrorKind::RateLimited);
    assert_eq!(parsed.retry_after_ms, Some(250));

    // And the new serialisation produces the same bytes (modulo
    // optional field ordering — `serde_json` writes structs in field
    // declaration order, which matches `legacy_json` here).
    let reserialized = serde_json::to_string(&parsed).unwrap();
    assert_eq!(reserialized, legacy_json);
}

#[test]
fn provider_error_kind_pairs_with_provider_error_variant_for_variant() {
    // Each ProviderError variant must produce a distinct
    // ProviderErrorKind via .kind(). Discovers accidentally collapsed
    // variants (e.g. someone reusing K::Internal for two different
    // ProviderError cases).
    let samples: Vec<(ProviderError, ProviderErrorKind)> = vec![
        (ProviderError::Auth("x".into()), ProviderErrorKind::Auth),
        (
            ProviderError::RateLimited { retry_after: None },
            ProviderErrorKind::RateLimited,
        ),
        (
            ProviderError::BudgetExceeded,
            ProviderErrorKind::BudgetExceeded,
        ),
        (
            ProviderError::InvalidRequest("bad".into()),
            ProviderErrorKind::InvalidRequest,
        ),
        (
            ProviderError::ModelOverloaded,
            ProviderErrorKind::ModelOverloaded,
        ),
        (ProviderError::Parse("p".into()), ProviderErrorKind::Parse),
        (
            ProviderError::Internal("i".into()),
            ProviderErrorKind::Internal,
        ),
    ];
    for (err, expected) in samples {
        assert_eq!(err.kind(), expected, "mapping wrong for {err:?}");
    }
}
