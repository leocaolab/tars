//! Collects an [`LlmService`] call into one [`ChatResponse`].

use futures::StreamExt;

use crate::{LlmError, LlmService};
use tars_types::{Caller, ChatRequest, ChatResponse, ChatResponseBuilder};

/// Drains `svc.call(req, ctx)` into one [`ChatResponse`]; a validation layer's filtered response
/// and summary arrive in the stream.
///
/// # Errors
///
/// Returns the call's error or the first error the stream yields.
pub async fn complete_async(
    svc: LlmService,
    req: ChatRequest,
    ctx: Caller,
) -> Result<ChatResponse, LlmError> {
    let mut stream = svc.call(req, ctx).await.result?;
    let mut builder = ChatResponseBuilder::new();
    while let Some(ev) = stream.next().await {
        builder.apply(ev?);
    }
    Ok(builder.finish())
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
