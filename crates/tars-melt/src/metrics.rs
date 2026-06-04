//! OpenTelemetry metrics bridge (the `otlp` feature).
//!
//! Converts the `llm.call.finished` / `llm.call.failed` `tracing` events
//! the pipeline already emits (see `tars-pipeline::telemetry`) into OTel
//! metric instruments — **no instrumentation added to the pipeline**,
//! the events are the source of truth. Exported over OTLP to the same
//! collector as traces.
//!
//! Instruments (attribute: `model`, and `outcome` = ok/error):
//! - `tars.llm.calls` (counter) — one per completed/failed call
//! - `tars.llm.latency_ms` (histogram) — end-to-end pipeline latency
//! - `tars.llm.tokens` (counter) — input+output tokens (finished only)

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::TelemetryError;

/// Build a batch OTLP meter provider for the given endpoint.
pub(crate) fn build_meter_provider(
    endpoint: &str,
    service: &str,
) -> Result<SdkMeterProvider, TelemetryError> {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| TelemetryError::OtlpExport {
            endpoint: endpoint.to_string(),
            reason: format!("metrics: {e}"),
        })?;
    let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(
        exporter,
        opentelemetry_sdk::runtime::Tokio,
    )
    .build();
    let resource =
        opentelemetry_sdk::Resource::new([KeyValue::new("service.name", service.to_string())]);
    Ok(SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build())
}

/// `tracing` layer that records OTel metrics off the pipeline's
/// `llm.call.*` events. Holds the instruments so each event is a cheap
/// `add`/`record`.
pub(crate) struct MetricsBridge {
    calls: Counter<u64>,
    latency_ms: Histogram<u64>,
    tokens: Counter<u64>,
}

impl MetricsBridge {
    pub(crate) fn new(provider: &SdkMeterProvider) -> Self {
        let meter = provider.meter("tars");
        Self {
            calls: meter.u64_counter("tars.llm.calls").build(),
            latency_ms: meter.u64_histogram("tars.llm.latency_ms").build(),
            tokens: meter.u64_counter("tars.llm.tokens").build(),
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for MetricsBridge {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut v = CallVisitor::default();
        event.record(&mut v);
        let model = v.model.unwrap_or_else(|| "unknown".to_string());
        match v.event.as_deref() {
            Some("llm.call.finished") => {
                let attrs = [
                    KeyValue::new("model", model.clone()),
                    KeyValue::new("outcome", "ok"),
                ];
                self.calls.add(1, &attrs);
                if let Some(ms) = v.elapsed_ms {
                    self.latency_ms.record(ms, &attrs);
                }
                let tokens = v.input_tokens.unwrap_or(0) + v.output_tokens.unwrap_or(0);
                if tokens > 0 {
                    self.tokens.add(tokens, &[KeyValue::new("model", model)]);
                }
            }
            Some("llm.call.failed") => {
                let attrs = [
                    KeyValue::new("model", model),
                    KeyValue::new("outcome", "error"),
                ];
                self.calls.add(1, &attrs);
                if let Some(ms) = v.elapsed_ms {
                    self.latency_ms.record(ms, &attrs);
                }
            }
            _ => {}
        }
    }
}

/// Pulls the fields we care about off an `llm.call.*` event. `event` and
/// `model` arrive as strings (string literal / `%Display`), the counts
/// as `u64`.
#[derive(Default)]
struct CallVisitor {
    event: Option<String>,
    model: Option<String>,
    elapsed_ms: Option<u64>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl Visit for CallVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "event" => self.event = Some(value.to_string()),
            "model" => self.model = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "elapsed_ms" => self.elapsed_ms = Some(value),
            "input_tokens" => self.input_tokens = Some(value),
            "output_tokens" => self.output_tokens = Some(value),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `model = %model` (Display) lands here as the rendered string;
        // capture it only if `record_str` didn't already.
        if field.name() == "model" && self.model.is_none() {
            self.model = Some(format!("{value:?}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::testing::metrics::InMemoryMetricExporter;
    use tracing_subscriber::prelude::*;

    // Multi-thread runtime: the PeriodicReader runs a background task,
    // and `force_flush()` blocks until it drains — on a current-thread
    // runtime that's a self-deadlock (the flush owns the only thread).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pipeline_events_become_otel_metrics() {
        let exporter = InMemoryMetricExporter::default();
        let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(
            exporter.clone(),
            opentelemetry_sdk::runtime::Tokio,
        )
        .build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let bridge = MetricsBridge::new(&provider);
        let subscriber = tracing_subscriber::registry().with(bridge);

        tracing::subscriber::with_default(subscriber, || {
            // Mirror tars-pipeline's telemetry event shape.
            tracing::info!(
                event = "llm.call.finished",
                model = "qwen",
                elapsed_ms = 1500u64,
                input_tokens = 11u64,
                output_tokens = 4u64,
            );
            tracing::warn!(
                event = "llm.call.failed",
                model = "qwen",
                elapsed_ms = 200u64,
            );
        });

        provider.force_flush().unwrap();
        let metrics = exporter.get_finished_metrics().unwrap();
        let names: Vec<String> = metrics
            .iter()
            .flat_map(|rm| &rm.scope_metrics)
            .flat_map(|sm| &sm.metrics)
            .map(|m| m.name.to_string())
            .collect();
        // All three instruments materialized from the two pipeline events
        // — i.e. the tracing → metrics bridge fired for both finished and
        // failed.
        assert!(
            names.iter().any(|n| n == "tars.llm.calls"),
            "got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "tars.llm.latency_ms"),
            "got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "tars.llm.tokens"),
            "got: {names:?}"
        );
    }
}
