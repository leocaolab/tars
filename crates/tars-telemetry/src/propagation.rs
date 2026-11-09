//! Writes and reads the W3C trace context (`traceparent`, `tracestate`) that carries a trace
//! across a process boundary.

use std::collections::HashMap;

use opentelemetry::propagation::TextMapPropagator as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// Returns the W3C trace-context headers of the current `tracing` span, for an outbound request:
/// its `traceparent` names the span as the parent. Outside a span the trace layer has seen, or
/// with no trace export installed, there is no context and it returns none.
pub fn trace_headers() -> Vec<(String, String)> {
    let mut carrier = HashMap::new();
    TraceContextPropagator::new().inject_context(&tracing::Span::current().context(), &mut carrier);
    carrier.into_iter().collect()
}

/// Makes `span` continue the trace an inbound request carried in its W3C trace-context headers
/// (`traceparent`, `tracestate`; names are matched without regard to case). Call it before the
/// span is entered: the spans opened inside, a tars LLM call's among them, then share that trace
/// id and hang under the caller's span. Headers with no valid `traceparent` leave `span` a root.
pub fn continue_trace<'a>(
    span: &tracing::Span,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) {
    let carrier: HashMap<String, String> = headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_string()))
        .collect();
    span.set_parent(TraceContextPropagator::new().extract(&carrier));
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
    use opentelemetry_sdk::trace::TracerProvider;
    use tracing_subscriber::prelude::*;

    #[test]
    fn inside_a_span_the_traceparent_names_that_span_and_outside_there_is_none() {
        let provider = TracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));

        let (outside, inside, context) = tracing::subscriber::with_default(subscriber, || {
            let outside = trace_headers();
            let span = tracing::info_span!("call");
            let inside = span.in_scope(trace_headers);
            (outside, inside, span.context())
        });

        assert!(outside.is_empty(), "{outside:?}");
        let span = context.span().span_context().clone();
        let traceparent = inside
            .iter()
            .find(|(name, _)| name == "traceparent")
            .map(|(_, value)| value.as_str());
        assert_eq!(
            traceparent,
            Some(format!("00-{}-{}-01", span.trace_id(), span.span_id()).as_str())
        );
    }

    #[test]
    fn a_span_given_an_inbound_traceparent_continues_that_trace_and_so_do_its_children() {
        let provider = TracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));

        let (request, child) = tracing::subscriber::with_default(subscriber, || {
            let request = tracing::info_span!("request");
            continue_trace(
                &request,
                [(
                    "TraceParent",
                    "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
                )],
            );
            let child = request.in_scope(|| tracing::info_span!("llm.call"));
            (request.context(), child.context())
        });

        let request = request.span().span_context().clone();
        assert_eq!(
            request.trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(child.span().span_context().trace_id(), request.trace_id());
    }

    #[test]
    fn headers_without_a_valid_traceparent_leave_the_span_a_root() {
        let provider = TracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));

        let context = tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request");
            continue_trace(&span, [("traceparent", "not-a-traceparent")]);
            span.in_scope(|| tracing::info_span!("child")).context()
        });

        let trace_id = context.span().span_context().trace_id().to_string();
        assert_ne!(trace_id, "00000000000000000000000000000000");
        assert_ne!(trace_id, "0af7651916cd43dd8448eb211c80319c");
    }
}
