//! Exports the `tracing` spans and events tars already emits to an OTLP collector (Jaeger, Tempo,
//! the OTel Collector), next to the same stderr logs as the path without export.

use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::{Tracer, TracerProvider};
use tracing::{Event, Subscriber};
use tracing_opentelemetry::OtelData;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{EnvFilter, FilterExt as _, LevelFilter, filter_fn};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

use crate::integers::SpanIntegers;
use crate::metrics::{MetricsBridge, build_meter_provider};
use crate::span_events::SpanEvents;
use crate::{
    Installation, OtlpExport, OtlpProviders, SampleRatio, TelemetryConfig, TelemetryError,
    TelemetryFormat, TelemetryGuard, TelemetryWarning, install_global, sanitize_service,
    stderr_colour,
};

/// Installs the subscriber that logs to stderr and exports spans and metrics over OTLP, and
/// returns a guard that flushes the exporters on drop. It must run inside a Tokio runtime,
/// because the batch exporter spawns a task.
pub(crate) fn install(
    config: &TelemetryConfig,
    export: &OtlpExport,
    filter: EnvFilter,
    span_events: FmtSpan,
    warnings: Vec<TelemetryWarning>,
) -> Result<Installation, TelemetryError> {
    let safe_service = sanitize_service(&config.service);
    let endpoint = export.endpoint.as_str();

    let tracer_provider = build_provider(endpoint, &safe_service, export.trace_sample_ratio)?;
    let tracer = tracer_provider.tracer("tars");
    let meter_provider = build_meter_provider(endpoint, &safe_service)?;

    // Each layer has its own filter: the logs follow the user's level, while trace and metric
    // export keep every INFO+ event (`llm.call.finished`), so a quiet run still exports.
    let fmt_layer = match config.format {
        TelemetryFormat::Pretty => tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(stderr_colour())
            .with_target(false)
            .with_span_events(span_events)
            .boxed(),
        TelemetryFormat::Json => tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(true)
            .with_span_events(span_events)
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .map_event_format(WithTraceIds)
            .boxed(),
    };

    let metrics_layer = MetricsBridge::new(&meter_provider).with_filter(LevelFilter::INFO);

    let subscriber = tracing_subscriber::registry()
        .with(fmt_layer.with_filter(filter))
        .with(trace_layer(tracer))
        .with(metrics_layer);
    let installed = install_global(
        subscriber.into(),
        TelemetryGuard::with_providers(OtlpProviders {
            tracer: tracer_provider,
            meter: meter_provider,
        }),
        config,
        warnings,
    )?;

    tracing::info!(
        service = %safe_service,
        format = ?config.format,
        otlp_endpoint = %endpoint,
        version = env!("CARGO_PKG_VERSION"),
        "telemetry initialized (OTLP trace + metric export enabled)",
    );
    Ok(installed)
}

/// Returns the layer that exports INFO+ spans with the events emitted in them. The spans are
/// tracing-opentelemetry's, with their integer attributes recorded again by [`SpanIntegers`]; the
/// events are [`SpanEvents`]'s, so tracing-opentelemetry is kept from recording them a second
/// time.
fn trace_layer<S>(tracer: Tracer) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let spans_only = || LevelFilter::INFO.and(filter_fn(|meta| meta.is_span()));
    tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(spans_only())
        .and_then(SpanIntegers.with_filter(spans_only()))
        .and_then(SpanEvents.with_filter(LevelFilter::INFO))
}

fn build_provider(
    endpoint: &str,
    service: &str,
    sample_ratio: SampleRatio,
) -> Result<TracerProvider, TelemetryError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| TelemetryError::OtlpExport {
            endpoint: endpoint.to_string(),
            source: crate::OtlpBuildError::Traces(e),
        })?;

    let resource = opentelemetry_sdk::Resource::new([opentelemetry::KeyValue::new(
        "service.name",
        service.to_string(),
    )]);

    let sampler = opentelemetry_sdk::trace::Sampler::ParentBased(Box::new(
        opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(sample_ratio.get()),
    ));

    Ok(TracerProvider::builder()
        .with_sampler(sampler)
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::Value;
    use opentelemetry::trace::Status;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use std::sync::{Arc, Mutex};
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    fn ids(trace_id: &str, span_id: &str) -> TraceIds {
        TraceIds {
            trace_id: trace_id.into(),
            span_id: span_id.into(),
        }
    }

    #[test]
    fn the_ids_lead_the_json_object() {
        let line = "{\"level\":\"INFO\",\"event\":\"x\"}\n";
        assert_eq!(
            with_trace_ids(
                line,
                &ids("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331")
            ),
            "{\"trace_id\":\"0af7651916cd43dd8448eb211c80319c\",\"span_id\":\"b7ad6b7169203331\",\"level\":\"INFO\",\"event\":\"x\"}\n"
        );
    }

    #[test]
    fn an_empty_object_takes_the_ids_without_a_dangling_comma() {
        assert_eq!(
            with_trace_ids("{}", &ids("t", "s")),
            "{\"trace_id\":\"t\",\"span_id\":\"s\"}"
        );
    }

    #[test]
    fn an_event_field_named_like_an_id_does_not_make_a_second_key() {
        let line = "{\"trace_id\":\"caller-supplied\",\"event\":\"x\",\"n\":1.5}\n";
        let out = with_trace_ids(line, &ids("t", "s"));
        assert_eq!(
            out,
            "{\"trace_id\":\"t\",\"span_id\":\"s\",\"event.trace_id\":\"caller-supplied\",\"event\":\"x\",\"n\":1.5}\n"
        );
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["trace_id"], "t");
    }

    #[test]
    fn a_line_that_is_not_an_object_is_left_alone() {
        assert_eq!(with_trace_ids("plain\n", &ids("t", "s")), "plain\n");
    }

    /// Collects what the fmt layer writes, so a test can read the log lines back.
    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Lines {
        fn json(&self) -> Vec<serde_json::Value> {
            let bytes = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    #[test]
    fn a_log_line_in_a_span_carries_that_span_s_otel_ids_and_one_outside_carries_none() {
        let provider = TracerProvider::builder().build();
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(move || writer.clone())
                    .json()
                    .flatten_event(true)
                    .map_event_format(WithTraceIds),
            )
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));

        let (outer_ctx, inner_ctx) = tracing::subscriber::with_default(subscriber, || {
            tracing::info!(event = "outside");
            let outer = tracing::info_span!("outer");
            let inner = outer.in_scope(|| tracing::info_span!("inner"));
            inner.in_scope(|| tracing::info!(event = "inside"));
            (outer.context(), inner.context())
        });

        let logged = lines.json();
        assert_eq!(logged.len(), 2, "{logged:?}");
        assert!(logged[0].get("trace_id").is_none(), "{}", logged[0]);
        let inner = inner_ctx.span().span_context().clone();
        assert_eq!(logged[1]["trace_id"], inner.trace_id().to_string());
        assert_eq!(logged[1]["span_id"], inner.span_id().to_string());
        assert_eq!(
            inner.trace_id(),
            outer_ctx.span().span_context().trace_id(),
            "a child span logs its root's trace id"
        );
        assert_eq!(logged[1]["trace_id"].as_str().map(str::len), Some(32));
        assert_eq!(logged[1]["span_id"].as_str().map(str::len), Some(16));
    }

    #[test]
    fn tracing_spans_bridge_into_the_otel_exporter() {
        // A scoped subscriber rather than the global install, so the test needs no collector and
        // does not touch the process-global subscriber.
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("test");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("unit_of_work", detail = "x").in_scope(|| {});
        });

        let _ = provider.force_flush();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1, "one tracing span → one exported OTel span");
        assert_eq!(spans[0].name, "unit_of_work");
    }

    #[test]
    fn a_span_event_is_named_by_its_event_field_and_keeps_its_numbers_typed() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(trace_layer(provider.tracer("test")));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("call").in_scope(|| {
                tracing::info!(
                    event = "llm.call.finished",
                    input_tokens = 3_u64,
                    past_i64 = u64::MAX,
                    delta = -2_i64,
                    ratio = 0.5_f64,
                    cached = true,
                    model = %"m",
                    "call done",
                );
            });
        });

        let _ = provider.force_flush();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        let events = &spans[0].events.events;
        assert_eq!(
            events.len(),
            1,
            "one tracing event, one span event: {events:?}"
        );
        assert_eq!(events[0].name, "llm.call.finished");
        let attribute = |key: &str| {
            events[0]
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.clone())
        };
        assert_eq!(attribute("input_tokens"), Some(Value::I64(3)));
        assert_eq!(
            attribute("past_i64"),
            Some(Value::from(u64::MAX.to_string()))
        );
        assert_eq!(attribute("delta"), Some(Value::I64(-2)));
        assert_eq!(attribute("ratio"), Some(Value::F64(0.5)));
        assert_eq!(attribute("cached"), Some(Value::Bool(true)));
        assert_eq!(attribute("model"), Some(Value::from("m".to_owned())));
        assert_eq!(
            attribute("message"),
            Some(Value::from("call done".to_owned()))
        );
        assert_eq!(attribute("event"), None);
    }

    /// The four sides of `i64`'s range, as a field of each integer type `tracing` records, and
    /// what each exports as.
    fn integer_edges() -> [(&'static str, Value); 4] {
        [
            ("at_i64_max", Value::I64(i64::MAX)),
            (
                "past_i64_max",
                Value::from((i64::MAX as u64 + 1).to_string()),
            ),
            ("u64_max", Value::from(u64::MAX.to_string())),
            (
                "below_i64_min",
                Value::from((i128::from(i64::MIN) - 1).to_string()),
            ),
        ]
    }

    fn value_of(attributes: &[opentelemetry::KeyValue], key: &str) -> Option<Value> {
        attributes
            .iter()
            .rev()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.clone())
    }

    #[test]
    fn span_attributes_and_span_events_export_integers_by_the_same_rule() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(trace_layer(provider.tracer("test")));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "call",
                at_i64_max = i64::MAX as u64,
                past_i64_max = i64::MAX as u64 + 1,
                u64_max = tracing::field::Empty,
                below_i64_min = i128::from(i64::MIN) - 1,
            );
            span.record("u64_max", u64::MAX);
            span.in_scope(|| {
                tracing::info!(
                    event = "llm.call.finished",
                    at_i64_max = i64::MAX as u64,
                    past_i64_max = i64::MAX as u64 + 1,
                    u64_max = u64::MAX,
                    below_i64_min = i128::from(i64::MIN) - 1,
                );
            });
        });

        let _ = provider.force_flush();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        let event = &spans[0].events.events[0].attributes;
        for (key, exported) in integer_edges() {
            assert_eq!(
                value_of(&spans[0].attributes, key),
                Some(exported.clone()),
                "span {key}"
            );
            assert_eq!(value_of(event, key), Some(exported), "event {key}");
        }
    }

    /// Datadog marks a span from OTLP as an error by its status code alone and takes the status
    /// description as the error message; `error.type` passes through as a span attribute.
    #[test]
    fn a_failed_call_exports_an_error_status_with_its_message_and_keeps_its_event_name() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(trace_layer(provider.tracer("test")));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "llm.call",
                otel.status_code = tracing::field::Empty,
                error.type = tracing::field::Empty,
            );
            span.record("error.type", "rate_limited");
            span.record("otel.status_code", "error");
            span.in_scope(|| {
                tracing::warn!(event = "llm.call.failed", error = %"429: slow down");
            });
        });

        let _ = provider.force_flush();
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans[0].status, Status::error("429: slow down"));
        assert_eq!(
            value_of(&spans[0].attributes, "error.type"),
            Some(Value::from("rate_limited"))
        );
        let event = &spans[0].events.events[0];
        assert_eq!(event.name, "llm.call.failed");
        assert_eq!(
            value_of(&event.attributes, "error"),
            Some(Value::from("429: slow down".to_owned()))
        );
    }

    #[test]
    fn a_debug_event_is_not_exported() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(trace_layer(provider.tracer("test")));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("call").in_scope(|| tracing::debug!(event = "llm.call.opened"));
        });

        let _ = provider.force_flush();
        let spans = exporter.get_finished_spans().unwrap();
        assert!(spans[0].events.events.is_empty(), "{:?}", spans[0].events);
    }
}

/// Writes each JSON log line with the OpenTelemetry ids of the span it was emitted in, as the
/// top-level `trace_id` and `span_id` a backend joins logs to traces by (Datadog reads them as
/// lowercase hex). A line outside any span, or in one the trace layer has not seen, is left as is.
pub(crate) struct WithTraceIds<F>(pub(crate) F);

impl<S, N, F> FormatEvent<S, N> for WithTraceIds<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let ids = ctx
            .event_scope()
            .and_then(|mut scope| scope.next())
            .and_then(|span| span.extensions().get::<OtelData>().and_then(otel_ids));
        let Some(ids) = ids else {
            return self.0.format_event(ctx, writer, event);
        };
        let mut line = String::new();
        self.0.format_event(ctx, Writer::new(&mut line), event)?;
        writer.write_str(&with_trace_ids(&line, &ids))
    }
}

/// The OpenTelemetry ids of one span, as lowercase hex.
#[derive(Debug, PartialEq, Eq)]
struct TraceIds {
    trace_id: String,
    span_id: String,
}

/// Returns the span's ids. A root span carries the trace id it started; a child inherits its
/// parent's, which the trace layer keeps in the parent context instead.
fn otel_ids(data: &OtelData) -> Option<TraceIds> {
    let span_id = data.builder.span_id?;
    let trace_id = match data.builder.trace_id {
        Some(id) => id,
        None => {
            let parent = data.parent_cx.span();
            let context = parent.span_context();
            context.is_valid().then(|| context.trace_id())?
        }
    };
    Some(TraceIds {
        trace_id: trace_id.to_string(),
        span_id: span_id.to_string(),
    })
}

/// Returns `line`, one JSON object, with the span's `trace_id` and `span_id` as its first keys. An
/// event field of the same name is kept as `event.trace_id` / `event.span_id`, so the line has one
/// key of each name and a backend joins it by the span's ids. A line that is not an object is
/// returned unchanged.
fn with_trace_ids(line: &str, ids: &TraceIds) -> String {
    let mut objects = serde_json::Deserializer::from_str(line).into_iter::<Members>();
    let Some(Ok(Members(members))) = objects.next() else {
        return line.to_string();
    };
    let after = &line[objects.byte_offset()..];
    let mut out = format!(
        "{{\"trace_id\":{},\"span_id\":{}",
        serde_json::Value::from(ids.trace_id.as_str()),
        serde_json::Value::from(ids.span_id.as_str()),
    );
    for (key, value) in members {
        let key = match key.as_str() {
            "trace_id" | "span_id" => format!("event.{key}"),
            _ => key,
        };
        out.push(',');
        out.push_str(&serde_json::Value::from(key).to_string());
        out.push(':');
        out.push_str(value.get());
    }
    out.push('}');
    out.push_str(after);
    out
}

/// The members of one JSON object in the order they were written, each value as its own text.
struct Members(Vec<(String, Box<serde_json::value::RawValue>)>);

impl<'de> serde::Deserialize<'de> for Members {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = Members;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Members, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry()? {
                    members.push(member);
                }
                Ok(Members(members))
            }
        }
        deserializer.deserialize_map(Visit)
    }
}
