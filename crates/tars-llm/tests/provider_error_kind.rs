//! Pins that each [`tars_llm::ProviderError`] variant maps to its own [`LlmErrorKind`]; the
//! kinds' wire spelling is pinned in tars-types.
use tars_llm::{CredentialError, ParseProblem, ProviderError, ProviderRefusal, RequestRefusal};
use tars_types::LlmErrorKind;

#[test]
fn provider_error_kind_pairs_with_provider_error_variant_for_variant() {
    // Each sampled variant maps to its own kind, so two variants collapsed onto one
    // kind fail here.
    let samples: Vec<(ProviderError, LlmErrorKind)> = vec![
        (
            ProviderError::Auth(CredentialError::EmptyKey),
            LlmErrorKind::Auth,
        ),
        (
            ProviderError::RateLimited { retry_after: None },
            LlmErrorKind::RateLimited,
        ),
        (
            ProviderError::InvalidRequest(RequestRefusal::Provider(ProviderRefusal {
                status: Some(400),
                message: "bad".into(),
            })),
            LlmErrorKind::InvalidRequest,
        ),
        (
            ProviderError::ModelOverloaded,
            LlmErrorKind::ModelOverloaded,
        ),
        (
            ProviderError::Parse {
                at: "a test response",
                line: None,
                problem: Box::new(ParseProblem::Missing("id")),
                raw: None,
            },
            LlmErrorKind::Parse,
        ),
        (ProviderError::Internal("i".into()), LlmErrorKind::Internal),
    ];
    for (err, expected) in samples {
        assert_eq!(err.kind(), expected, "mapping wrong for {err:?}");
    }
}
