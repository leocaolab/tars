//! OpenTelemetry OTLP trace export (the `otlp` feature).
//!
//! Bridges the `tracing` spans the whole `tars` stack already emits into
//! the OpenTelemetry SDK and ships them to an OTLP collector
//! (Jaeger / Tempo / Datadog / the OTel Collector). Off unless built
//! with `--features otlp` AND `OTEL_EXPORTER_OTLP_ENDPOINT` is set, so
//! the default build pays nothing (no tonic/grpc stack, no runtime
//! exporter task).
//!
//! Composition: `registry().with(env_filter).with(fmt_layer)
//! .with(otel_layer)` — the same stderr fmt logs as the no-export path,
//! plus a parallel span pipeline to the collector.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;

use crate::{TelemetryConfig, TelemetryError, TelemetryFormat, TelemetryGuard, sanitize_service};

/// Install the layered subscriber (stderr fmt + OTLP span export) and
/// return a guard that flushes the exporter on drop. Must run inside a
/// Tokio runtime — the batch exporter spawns a background task.
pub(crate) fn install(
    config: &TelemetryConfig,
    filter: EnvFilter,
    span_events: FmtSpan,
    endpoint: &str,
) -> Result<TelemetryGuard, TelemetryError> {
    let safe_service = sanitize_service(&config.service);
    let provider = build_provider(endpoint, &safe_service)?;
    let tracer = provider.tracer("tars");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // Mirror the no-export path's fmt config exactly; box so Pretty/Json
    // share one type in the `.with(...)` chain.
    let fmt_layer = match config.format {
        TelemetryFormat::Pretty => tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
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
            .boxed(),
    };

    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .try_init();

    if installed.is_err() {
        // Another subscriber won the race; don't leak the exporter task.
        let _ = provider.shutdown();
        return Err(TelemetryError::AlreadyInstalled {
            service: safe_service,
            level: config.level.clone(),
        });
    }

    tracing::info!(
        service = %safe_service,
        format = ?config.format,
        otlp_endpoint = %endpoint,
        version = env!("CARGO_PKG_VERSION"),
        "telemetry initialized (OTLP trace export enabled)",
    );
    Ok(TelemetryGuard::with_provider(provider))
}

/// Build the batch OTLP tracer provider, tagging spans with
/// `service.name` so the collector groups them under this binary.
fn build_provider(endpoint: &str, service: &str) -> Result<TracerProvider, TelemetryError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| TelemetryError::OtlpExport {
            endpoint: endpoint.to_string(),
            reason: e.to_string(),
        })?;

    let resource = opentelemetry_sdk::Resource::new([opentelemetry::KeyValue::new(
        "service.name",
        service.to_string(),
    )]);

    Ok(TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;

    #[test]
    fn tracing_spans_bridge_into_the_otel_exporter() {
        // The core contract: a `tracing` span becomes an OTel span at the
        // exporter. Uses an in-memory exporter + a *scoped* subscriber
        // (`with_default`, not the global install) so the test needs no
        // collector and doesn't fight the process-global subscriber.
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
}
