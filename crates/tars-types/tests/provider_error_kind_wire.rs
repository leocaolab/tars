//! Pins the wire spelling of [`tars_types::LlmErrorKind`], which event stores,
//! Python and persisted `RetryAttempt` rows read; renaming a variant fails here.
use tars_types::{LlmErrorKind, RetryAttempt};

/// Returns every kind with the wire string consumers rely on.
fn expected_kind_wire_pairs() -> Vec<(LlmErrorKind, &'static str)> {
    use LlmErrorKind as K;
    vec![
        (K::Auth, "auth"),
        (K::RateLimited, "rate_limited"),
        (K::BudgetExceeded, "budget_exceeded"),
        (K::InvalidRequest, "invalid_request"),
        (K::ContentFiltered, "content_filtered"),
        (K::ContextTooLong, "context_too_long"),
        (K::ModelOverloaded, "model_overloaded"),
        (K::ServerError, "server_error"),
        (K::RequestTimeout, "request_timeout"),
        (K::ProviderTimedOut, "provider_timed_out"),
        (K::Unavailable, "unavailable"),
        (K::Network, "network"),
        (K::StreamCut, "stream_cut"),
        (K::Unfinished, "unfinished"),
        (K::Parse, "parse"),
        (K::CliSubprocessDied, "cli_subprocess_died"),
        (K::TimedOut, "timed_out"),
        (K::MalformedToolCall, "malformed_tool_call"),
        (K::TruncatedToolCall, "truncated_tool_call"),
        (K::IncompleteToolCall, "incomplete_tool_call"),
        (K::Incompatible, "incompatible"),
        (K::ValidationFailed, "validation_failed"),
        (K::ValidationCancelled, "validation_cancelled"),
        (K::BudgetStoreFailed, "budget_store_failed"),
        (K::BudgetBalanceCorrupt, "budget_balance_corrupt"),
        (K::Cancelled, "cancelled"),
        (K::Encode, "encode"),
        (K::InvalidUrl, "invalid_url"),
        (K::InvalidHeader, "invalid_header"),
        (K::DaemonScript, "daemon_script"),
        (K::Spawn, "spawn"),
        (K::Io, "io"),
        (K::DaemonFailed, "daemon_failed"),
        (K::DaemonExited, "daemon_exited"),
        (K::Sandbox, "sandbox"),
        (K::BatchItemCanceled, "batch_item_canceled"),
        (K::BatchItemExpired, "batch_item_expired"),
        (K::BatchItemFailed, "batch_item_failed"),
        (K::Internal, "internal"),
    ]
}

#[test]
fn as_str_matches_serde_serialised_form_for_every_variant() {
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
        let parsed: LlmErrorKind = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, kind, "round-trip failed for {kind:?} via {s}");
    }
}

#[test]
fn deserialise_rejects_unknown_variant() {
    let err = serde_json::from_str::<LlmErrorKind>(r#""not_a_real_kind""#).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not_a_real_kind") || msg.contains("unknown variant"),
        "unexpected serde error: {msg}"
    );
}

#[test]
fn retry_attempt_wire_form_unchanged_from_pre_typing() {
    // A hand-written row, as persisted retry attempts are stored.
    let legacy_json = r#"{"error_kind":"rate_limited","retry_after_ms":250}"#;
    let parsed: RetryAttempt = serde_json::from_str(legacy_json).unwrap();
    assert_eq!(parsed.error_kind, LlmErrorKind::RateLimited);
    assert_eq!(parsed.retry_after_ms, Some(250));

    // `serde_json` writes fields in declaration order, which this row follows.
    let reserialized = serde_json::to_string(&parsed).unwrap();
    assert_eq!(reserialized, legacy_json);
}
