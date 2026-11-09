//! Binds the calls of `tars probe` and `tars bench`, which show or time what a provider does on
//! its first attempt. The telemetry layer gives each call the span every call gets; no retry
//! layer stands between the command and the provider, since a retry would hide the first failure
//! and add its backoff to the time measured.

use std::sync::Arc;

use tars_llm::{LlmProvider, LlmService, TelemetryMiddleware};

/// Returns `provider` bound to `model` under the telemetry layer alone.
pub(crate) fn service(provider: Arc<dyn LlmProvider>, model: &str) -> LlmService {
    LlmService::builder(provider, model)
        .layer(TelemetryMiddleware::new())
        .build()
}

#[cfg(test)]
pub(crate) mod testkit {
    use std::sync::{Arc, Mutex};

    use tracing::span::{Attributes, Id};
    use tracing::subscriber::Subscriber;
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::registry::LookupSpan;

    /// Collects the `gen_ai.request.model` of every `llm.call` span opened.
    #[derive(Clone, Default)]
    pub(crate) struct CallSpans(Arc<Mutex<Vec<String>>>);

    impl CallSpans {
        pub(crate) fn models(&self) -> Vec<String> {
            self.0.lock().expect("span list lock").clone()
        }
    }

    impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for CallSpans {
        fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
            if attrs.metadata().name() != "llm.call" {
                return;
            }
            let mut model = RequestModel::default();
            attrs.record(&mut model);
            self.0.lock().expect("span list lock").push(model.0);
        }
    }

    #[derive(Default)]
    struct RequestModel(String);

    impl tracing::field::Visit for RequestModel {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "gen_ai.request.model" {
                self.0 = value.to_string();
            }
        }
        fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use futures::StreamExt;
    use tars_llm::{CannedResponse, MockProvider};
    use tars_types::{Caller, ChatRequest, ProviderId, TenantId};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry::Registry;

    use super::testkit::CallSpans;
    use super::*;

    #[tokio::test]
    async fn one_call_opens_one_call_span_for_its_model() {
        let spans = CallSpans::default();
        let _guard = tracing::subscriber::set_default(Registry::default().with(spans.clone()));
        let provider = MockProvider::new(ProviderId::new("mock"), CannedResponse::text("hello"));

        let stream = service(provider, "probe-model")
            .call(
                ChatRequest::user("hi"),
                Caller::new(
                    TenantId::from_str("acme").expect("a plain tenant id parses"),
                    tars_types::TraceId::from(uuid::Uuid::new_v4()),
                ),
            )
            .await
            .result
            .expect("the mock opens a stream");
        let events = stream.collect::<Vec<_>>().await;

        assert!(events.iter().all(Result::is_ok));
        assert_eq!(spans.models(), ["probe-model"]);
    }
}
