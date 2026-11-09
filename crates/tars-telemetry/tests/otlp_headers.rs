//! Checks that the headers in `OTEL_EXPORTER_OTLP_HEADERS` ride on both OTLP exports, the spans
//! and the metrics, so a collector that wants a key (Datadog's `DD-API-KEY`) gets one.
//!
//! `std::env::set_var` is `unsafe` in Rust 2024 and the workspace forbids `unsafe`, so the test
//! re-execs its own binary with the variables set on the child `Command`; the parent is the
//! collector.
#![cfg(feature = "otlp")]

use std::net::SocketAddr;
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex, PoisonError};

use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

const CHILD_MARKER: &str = "TARS_TELEMETRY_OTLP_HEADERS_CHILD";
const TEST_NAME: &str = "the_configured_headers_reach_the_collector_on_spans_and_metrics";
const HEADER: &str = "x-tars-probe";
const VALUE: &str = "present";

/// Each export the collector took, by which service took it, with the probe header's value if
/// the request carried one.
#[derive(Default)]
struct Seen {
    traces: Vec<Option<String>>,
    metrics: Vec<Option<String>>,
}

#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Seen>>);

fn probe_value<T>(request: &Request<T>) -> Option<String> {
    request
        .metadata()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[tonic::async_trait]
impl TraceService for Collector {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        let seen = probe_value(&request);
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .traces
            .push(seen);
        Ok(Response::new(ExportTraceServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl MetricsService for Collector {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        let seen = probe_value(&request);
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .metrics
            .push(seen);
        Ok(Response::new(ExportMetricsServiceResponse::default()))
    }
}

/// Installs telemetry from the environment the parent gave, emits one span and one call event,
/// and drops the guard, which flushes both exporters before the process exits.
fn export_once() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = rt.enter();
    let env = tars_telemetry::TelemetryConfig::from_env("tars-telemetry", 0, None);
    assert!(env.warnings.is_empty(), "{:?}", env.warnings);
    let installed = tars_telemetry::init(env.config).unwrap();

    tracing::info_span!("probe").in_scope(|| {
        tracing::info!(
            event = "llm.call.finished",
            model = "probe-model",
            latency_ms = 1_u64,
            "probe call"
        );
    });
    drop(installed.guard);
}

#[test]
fn the_configured_headers_reach_the_collector_on_spans_and_metrics() {
    if std::env::var(CHILD_MARKER).is_ok() {
        export_once();
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let collector = Collector::default();
    let addr: SocketAddr = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tonic::transport::Server::builder()
            .add_service(TraceServiceServer::new(collector.clone()))
            .add_service(MetricsServiceServer::new(collector.clone()))
            .serve_with_incoming(TcpListenerStream::new(listener));
        tokio::spawn(server);
        addr
    });

    let exe = std::env::current_exe().unwrap();
    let output = StdCommand::new(exe)
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_MARKER, "1")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{addr}"))
        .env("OTEL_EXPORTER_OTLP_HEADERS", format!("{HEADER}={VALUE}"))
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The collector records a request before it replies, and the child's flush waits for the
    // reply, so every export is recorded by the time the child has exited.
    let seen = collector.0.lock().unwrap_or_else(PoisonError::into_inner);
    let want = Some(VALUE.to_string());
    assert!(
        !seen.traces.is_empty(),
        "no span export reached the collector"
    );
    assert!(
        !seen.metrics.is_empty(),
        "no metric export reached the collector"
    );
    assert!(seen.traces.iter().all(|v| *v == want), "{:?}", seen.traces);
    assert!(
        seen.metrics.iter().all(|v| *v == want),
        "{:?}",
        seen.metrics
    );
}
