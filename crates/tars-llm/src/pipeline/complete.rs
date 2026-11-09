//! Collects an [`LlmService`] call into one [`ChatResponse`].

use futures::StreamExt;

use crate::{LlmError, LlmService, ProviderError};
use tars_types::{Caller, ChatRequest, ChatResponse, ChatResponseBuilder};

/// Drains `svc.call(req, ctx)` into one [`ChatResponse`]; a validation layer's filtered response
/// and summary arrive in the stream.
///
/// # Errors
///
/// Returns the call's error or the first error the stream yields; a stream that breaks the event
/// protocol or never becomes a response fails as the provider's error of that kind.
pub async fn complete_async(
    svc: LlmService,
    req: ChatRequest,
    ctx: Caller,
) -> Result<ChatResponse, LlmError> {
    let mut stream = svc.call(req, ctx).await.result?;
    let mut builder = ChatResponseBuilder::new();
    while let Some(ev) = stream.next().await {
        builder.apply(ev?).map_err(ProviderError::from)?;
    }
    Ok(builder.finish().map_err(ProviderError::from)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::backends::mock::{CannedResponse, MockProvider};
    use crate::{ChainOptions, LlmService};
    use std::sync::Arc;
    use tars_types::ProviderId;

    #[tokio::test]
    async fn complete_async_drains_stream_into_response() {
        let provider = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hello"));
        let pipeline = LlmService::default_chain(provider, "p", ChainOptions::default());
        let svc: LlmService = pipeline;

        let req = ChatRequest::user("ping");
        let ctx = Caller::test_default();
        let resp = complete_async(svc, req, ctx).await.expect("call succeeds");

        assert_eq!(resp.text, "hello");
    }

    #[tokio::test]
    async fn a_stream_that_breaks_the_event_protocol_fails_the_call() {
        let provider = MockProvider::new(
            ProviderId::new("p"),
            CannedResponse::Sequence(vec![
                tars_types::ChatEvent::started("m"),
                tars_types::ChatEvent::ToolCallArgsDelta {
                    index: 0,
                    args_delta: "{}".into(),
                },
                tars_types::ChatEvent::Finished {
                    stop_reason: tars_types::StopReason::ToolUse,
                    usage: None,
                },
            ]),
        );
        let svc = LlmService::of(provider, "m");
        let got = complete_async(svc, ChatRequest::user("ping"), Caller::test_default()).await;
        assert!(
            matches!(
                &got,
                Err(e) if e.kind() == tars_types::LlmErrorKind::StreamViolation
            ),
            "{got:?}"
        );
    }

    #[tokio::test]
    async fn complete_async_substitutes_filtered_response() {
        use crate::{MaxLengthValidator, OutputValidator};

        let provider = MockProvider::new(ProviderId::new("p"), CannedResponse::text("hello world"));
        let opts = ChainOptions {
            validators: vec![
                Arc::new(MaxLengthValidator::truncate_above(5)) as Arc<dyn OutputValidator>
            ],
            ..Default::default()
        };
        let pipeline = LlmService::default_chain(provider, "p", opts);
        let svc: LlmService = pipeline;

        let req = ChatRequest::user("ping");
        let ctx = Caller::test_default();
        let resp = complete_async(svc, req, ctx).await.expect("call succeeds");

        assert_eq!(resp.text, "hello"); // post-Filter
    }
}
