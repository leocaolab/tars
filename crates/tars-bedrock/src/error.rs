//! Classify an AWS SDK error into the canonical [`ProviderError`]
//! (Doc 31 §8.4). CLAUDE.md #1: every arm carries the SDK's own message
//! string — never a `parse_failed`/`unknown` sentinel. The service's
//! real words ("you don't have access to the model with the specified
//! model ID") reach the operator classified, not swallowed.

use aws_sdk_bedrockruntime::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_bedrockruntime::operation::converse::ConverseError;
use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamError;
use aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError;
use tars_types::ProviderError;

/// Map a `SdkError<ConverseError>` (the error type from
/// `client.converse().send()`) into a typed [`ProviderError`].
///
/// Non-service failures (timeout / dispatch / construction / response)
/// have no service body, so we render the SDK's own `Display` chain
/// (via `DisplayErrorContext`, which walks `source()`) to keep the real
/// cause — a DNS failure, a TLS error, a bad region — rather than a bare
/// "network error".
pub fn classify_sdk_error<R>(err: SdkError<ConverseError, R>) -> ProviderError
where
    R: std::fmt::Debug,
{
    match into_service_error_or_context(err) {
        Ok(service_err) => classify_service_error(service_err),
        Err(other) => ProviderError::Internal(other),
    }
}

/// Split a `SdkError` into either its typed service error or a rendered
/// message for the non-service cases. Kept separate so the service-error
/// classification is unit-testable without constructing a full
/// `SdkError` (which needs an HTTP response).
fn into_service_error_or_context<R>(err: SdkError<ConverseError, R>) -> Result<ConverseError, String>
where
    R: std::fmt::Debug,
{
    match err {
        SdkError::ServiceError(ctx) => Ok(ctx.into_err()),
        // Timeout / dispatch (connect, DNS, TLS) / construction / a
        // malformed response: no service message. Render the full
        // source chain so the operator sees the real cause.
        other => Err(format!(
            "{}",
            aws_sdk_bedrockruntime::error::DisplayErrorContext(&other)
        )),
    }
}

/// Classify the typed Bedrock `ConverseError` service variant, carrying
/// the service message (Doc 31 §8.4).
pub fn classify_service_error(err: ConverseError) -> ProviderError {
    // The service message is the operator-facing truth; fall back to the
    // error code, then the variant Debug, so we never emit an empty string.
    let msg = err
        .meta()
        .message()
        .map(str::to_owned)
        .or_else(|| err.meta().code().map(str::to_owned))
        .unwrap_or_else(|| format!("{err:?}"));

    match err {
        ConverseError::AccessDeniedException(_) => ProviderError::Auth(msg),
        ConverseError::ThrottlingException(_) => ProviderError::RateLimited { retry_after: None },
        ConverseError::ModelNotReadyException(_)
        | ConverseError::ServiceUnavailableException(_) => ProviderError::ModelOverloaded,
        // ValidationException covers malformed request AND model-not-enabled
        // ("model ID isn't supported" / "access not granted"). Keep the
        // service message; do not fabricate token counts we don't have.
        ConverseError::ValidationException(_) => ProviderError::InvalidRequest(msg),
        ConverseError::ResourceNotFoundException(_) => ProviderError::InvalidRequest(msg),
        // Model-side / timeout / internal → Internal, message preserved.
        ConverseError::ModelErrorException(_)
        | ConverseError::ModelTimeoutException(_)
        | ConverseError::InternalServerException(_) => ProviderError::Internal(msg),
        // #[non_exhaustive] + Unhandled: still carry the real message.
        _ => ProviderError::Internal(msg),
    }
}

/// The operator-facing message for a streaming service error: the
/// service's own words first, then the error code, and only as a last
/// resort a generic note — never an empty string (CLAUDE.md #1).
fn service_message<E: ProvideErrorMetadata + std::fmt::Debug>(err: &E) -> String {
    // Mirror the unary `classify_service_error` fallback chain: the parsed
    // service message (present on real wire errors) → the error code → the
    // variant `Debug` (which still carries the exception's `message` field,
    // e.g. for hand-built errors whose `ErrorMetadata` is empty). Never an
    // empty or fabricated string.
    err.meta()
        .message()
        .map(str::to_owned)
        .or_else(|| err.meta().code().map(str::to_owned))
        .unwrap_or_else(|| format!("{err:?}"))
}

/// Classify the error from `converse_stream().send()`
/// (`SdkError<ConverseStreamError, _>`). Same taxonomy as the unary path
/// plus `ModelStreamErrorException`; every arm carries the real service
/// message (Doc 31 §8.4, CLAUDE.md #1).
pub fn classify_stream_send_error<R>(err: SdkError<ConverseStreamError, R>) -> ProviderError
where
    R: std::fmt::Debug,
{
    match err {
        SdkError::ServiceError(ctx) => classify_stream_service_error(ctx.into_err()),
        // No service body (timeout / dispatch / construction / bad
        // response): render the full source chain, not a bare label.
        other => ProviderError::Internal(format!(
            "{}",
            aws_sdk_bedrockruntime::error::DisplayErrorContext(&other)
        )),
    }
}

/// The typed `ConverseStreamError` → [`ProviderError`], carrying the
/// service message. Split out so it is unit-testable without building a
/// full `SdkError` (which needs an HTTP response).
pub fn classify_stream_service_error(err: ConverseStreamError) -> ProviderError {
    let msg = service_message(&err);
    match err {
        ConverseStreamError::AccessDeniedException(_) => ProviderError::Auth(msg),
        ConverseStreamError::ThrottlingException(_) => ProviderError::RateLimited { retry_after: None },
        ConverseStreamError::ModelNotReadyException(_)
        | ConverseStreamError::ServiceUnavailableException(_) => ProviderError::ModelOverloaded,
        ConverseStreamError::ValidationException(_)
        | ConverseStreamError::ResourceNotFoundException(_) => ProviderError::InvalidRequest(msg),
        ConverseStreamError::ModelErrorException(_)
        | ConverseStreamError::ModelTimeoutException(_)
        | ConverseStreamError::ModelStreamErrorException(_)
        | ConverseStreamError::InternalServerException(_) => ProviderError::Internal(msg),
        _ => ProviderError::Internal(msg),
    }
}

/// Classify a mid-stream event error from `stream.recv()`
/// (`SdkError<ConverseStreamOutputError, _>`) — the smaller error surface
/// the model can raise *after* the stream opened. Message preserved.
pub fn classify_stream_event_error<R>(err: SdkError<ConverseStreamOutputError, R>) -> ProviderError
where
    R: std::fmt::Debug,
{
    match err {
        SdkError::ServiceError(ctx) => classify_stream_output_service_error(ctx.into_err()),
        other => ProviderError::Internal(format!(
            "{}",
            aws_sdk_bedrockruntime::error::DisplayErrorContext(&other)
        )),
    }
}

/// The typed `ConverseStreamOutputError` → [`ProviderError`]. Testable
/// without an `SdkError`, like [`classify_stream_service_error`].
pub fn classify_stream_output_service_error(err: ConverseStreamOutputError) -> ProviderError {
    let msg = service_message(&err);
    match err {
        ConverseStreamOutputError::ThrottlingException(_) => {
            ProviderError::RateLimited { retry_after: None }
        }
        ConverseStreamOutputError::ServiceUnavailableException(_) => ProviderError::ModelOverloaded,
        ConverseStreamOutputError::ValidationException(_) => ProviderError::InvalidRequest(msg),
        ConverseStreamOutputError::ModelStreamErrorException(_)
        | ConverseStreamOutputError::InternalServerException(_) => ProviderError::Internal(msg),
        _ => ProviderError::Internal(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_bedrockruntime::types::error::ValidationException;

    #[test]
    fn validation_exception_is_invalid_request_and_carries_message() {
        // E2E-4: a synthetic ValidationException with the real service
        // wording must classify as InvalidRequest AND keep the message
        // (no sentinel).
        let ve = ValidationException::builder()
            .message("you don't have access to the model with the specified model ID")
            .build();
        let err = classify_service_error(ConverseError::ValidationException(ve));
        match err {
            ProviderError::InvalidRequest(m) => {
                assert!(
                    m.contains("model ID"),
                    "message substring must survive classification, got: {m}"
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn access_denied_is_auth_and_carries_message() {
        let ade = aws_sdk_bedrockruntime::types::error::AccessDeniedException::builder()
            .message("not authorized to perform bedrock:InvokeModel")
            .build();
        match classify_service_error(ConverseError::AccessDeniedException(ade)) {
            ProviderError::Auth(m) => assert!(m.contains("bedrock:InvokeModel")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn throttling_is_rate_limited() {
        let te = aws_sdk_bedrockruntime::types::error::ThrottlingException::builder()
            .message("rate exceeded")
            .build();
        assert!(matches!(
            classify_service_error(ConverseError::ThrottlingException(te)),
            ProviderError::RateLimited { .. }
        ));
    }

    #[test]
    fn stream_send_validation_is_invalid_request_and_carries_message() {
        // Streaming path preserves the same taxonomy + message as unary.
        let ve = ValidationException::builder()
            .message("model ID isn't supported for streaming")
            .build();
        match classify_stream_service_error(ConverseStreamError::ValidationException(ve)) {
            ProviderError::InvalidRequest(m) => assert!(m.contains("streaming"), "got: {m}"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn stream_event_model_stream_error_is_internal_with_message() {
        // A mid-stream ModelStreamError must surface as Internal with the
        // service's own words, never a sentinel (CLAUDE.md #1).
        let mse = aws_sdk_bedrockruntime::types::error::ModelStreamErrorException::builder()
            .message("connection reset mid-stream")
            .build();
        match classify_stream_output_service_error(
            ConverseStreamOutputError::ModelStreamErrorException(mse),
        ) {
            ProviderError::Internal(m) => assert!(m.contains("connection reset"), "got: {m}"),
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
