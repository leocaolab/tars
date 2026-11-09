//! Turns the `llm.call.finished` and `llm.call.failed` `tracing` events that tars-llm's
//! telemetry middleware emits into OTLP metrics, so the pipeline carries no metric code of its
//! own:
//!
//! - `tars.llm.calls`, a counter by `model` and `outcome` (`ok` or `error`); an event without a
//!   `model` field is counted without the `model` attribute;
//! - `tars.llm.latency_ms`, a histogram of the event's `elapsed_ms`, by the same attributes;
//! - `tars.llm.tokens`, a counter of input plus output tokens of finished calls, by `model`;
//! - `tars.llm.tokens_unreported`, a counter of finished calls that reported no token counts, by
//!   `model`, whose tokens are missing from `tars.llm.tokens`;
//! - `tars.metrics.cardinality_overflow`, a counter by `key` of the events whose value for that
//!   attribute was collapsed into the overflow bucket, which is the alert that a label is
//!   running away.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use std::num::NonZeroUsize;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::TelemetryError;

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
            source: crate::OtlpBuildError::Metrics(e),
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

pub(crate) struct MetricsBridge {
    calls: Counter<u64>,
    latency_ms: Histogram<u64>,
    tokens: Counter<u64>,
    tokens_unreported: Counter<u64>,
    cardinality_overflow: Counter<u64>,
    cardinality: CardinalityLimit,
}

impl MetricsBridge {
    pub(crate) fn new(provider: &SdkMeterProvider) -> Self {
        let meter = provider.meter("tars");
        Self {
            calls: meter.u64_counter("tars.llm.calls").build(),
            latency_ms: meter.u64_histogram("tars.llm.latency_ms").build(),
            tokens: meter.u64_counter("tars.llm.tokens").build(),
            tokens_unreported: meter.u64_counter("tars.llm.tokens_unreported").build(),
            cardinality_overflow: meter
                .u64_counter("tars.metrics.cardinality_overflow")
                .build(),
            cardinality: CardinalityLimit::new(DEFAULT_CARDINALITY_BUDGET),
        }
    }

    /// Returns the label `value` gets under `key`, counting it when it collapsed into the
    /// overflow bucket.
    fn label(&self, key: &'static str, value: &str) -> String {
        match self.cardinality.bucket(key, value) {
            Label::Kept(label) => label,
            Label::Overflow => {
                self.cardinality_overflow
                    .add(1, &[KeyValue::new("key", key)]);
                OVERFLOW_BUCKET.to_string()
            }
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for MetricsBridge {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut v = CallVisitor::default();
        event.record(&mut v);
        let Some(call) = v.event else {
            return;
        };
        let model = v
            .model
            .map(|model| KeyValue::new("model", self.label("model", &model)));
        let attrs: Vec<KeyValue> = model
            .iter()
            .cloned()
            .chain([KeyValue::new("outcome", call.outcome())])
            .collect();
        self.calls.add(1, &attrs);
        if let Some(ms) = v.elapsed_ms {
            self.latency_ms.record(ms, &attrs);
        }
        if call == CallEvent::Finished {
            let model = Vec::from_iter(model);
            match reported_tokens(v.input_tokens, v.output_tokens) {
                Some(tokens) => self.tokens.add(tokens, &model),
                None => self.tokens_unreported.add(1, &model),
            }
        }
    }
}

/// Returns a finished call's input plus output tokens, or `None` unless both were reported: half
/// a count added to the total would read as a cheap call.
fn reported_tokens(input: Option<u64>, output: Option<u64>) -> Option<u64> {
    Some(input?.saturating_add(output?))
}

/// The two `llm.call.*` events tars-llm's telemetry middleware emits that end a call; the
/// strings are the value of their `event` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallEvent {
    Finished,
    Failed,
}

impl CallEvent {
    fn parse(event: &str) -> Option<Self> {
        match event {
            "llm.call.finished" => Some(Self::Finished),
            "llm.call.failed" => Some(Self::Failed),
            _ => None,
        }
    }

    fn outcome(self) -> &'static str {
        match self {
            Self::Finished => "ok",
            Self::Failed => "error",
        }
    }
}

/// The fields of an `llm.call.*` event the metrics use.
#[derive(Default)]
struct CallVisitor {
    event: Option<CallEvent>,
    model: Option<String>,
    elapsed_ms: Option<u64>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl Visit for CallVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "event" => self.event = CallEvent::parse(value),
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
        // `model = %model` arrives here, rendered through Display.
        if field.name() == "model" && self.model.is_none() {
            self.model = Some(format!("{value:?}"));
        }
    }
}

/// How many distinct values a metric attribute key may take before further ones collapse into
/// [`OVERFLOW_BUCKET`].
const DEFAULT_CARDINALITY_BUDGET: NonZeroUsize = NonZeroUsize::new(100).expect("100 is not zero");

const OVERFLOW_BUCKET: &str = "__over_cardinality__";

/// Caps how many distinct values each metric attribute key takes, so an id or a runaway model
/// string leaked into a label costs the budget plus one overflow series rather than a metrics
/// backend.
struct CardinalityLimit {
    budget: NonZeroUsize,
    seen: std::sync::Mutex<
        std::collections::HashMap<&'static str, std::collections::HashSet<String>>,
    >,
}

impl CardinalityLimit {
    fn new(budget: NonZeroUsize) -> Self {
        Self {
            budget,
            seen: std::sync::Mutex::default(),
        }
    }

    /// Keeps `value` when it was seen before or `key` still has budget, and collapses it when
    /// `key` has used its budget on other values.
    fn bucket(&self, key: &'static str, value: &str) -> Label {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let set = seen.entry(key).or_default();
        if set.contains(value) {
            return Label::Kept(value.to_string());
        }
        if set.len() < self.budget.get() {
            set.insert(value.to_string());
            return Label::Kept(value.to_string());
        }
        Label::Overflow
    }
}

/// The value an attribute is exported with.
#[derive(Debug, PartialEq, Eq)]
enum Label {
    Kept(String),
    /// The key's budget was spent on other values; it is exported as [`OVERFLOW_BUCKET`].
    Overflow,
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::testing::metrics::InMemoryMetricExporter;
    use tracing_subscriber::prelude::*;

    // A multi-thread runtime, because `force_flush()` blocks until the PeriodicReader's task
    // drains, which on a current-thread runtime deadlocks.
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

        // The field names are tars-llm's telemetry events'; a name that drifted from theirs
        // would leave this test measuring nothing.
        tracing::subscriber::with_default(subscriber, || {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_finished_call_without_usage_is_counted_as_unreported() {
        let exporter = InMemoryMetricExporter::default();
        let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(
            exporter.clone(),
            opentelemetry_sdk::runtime::Tokio,
        )
        .build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let subscriber = tracing_subscriber::registry().with(MetricsBridge::new(&provider));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                event = "llm.call.finished",
                model = "qwen",
                elapsed_ms = 9u64
            );
        });

        provider.force_flush().unwrap();
        let names: Vec<String> = exporter
            .get_finished_metrics()
            .unwrap()
            .iter()
            .flat_map(|rm| &rm.scope_metrics)
            .flat_map(|sm| &sm.metrics)
            .map(|m| m.name.to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "tars.llm.tokens_unreported"),
            "got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "tars.llm.tokens"),
            "no tokens were reported, so none are counted: {names:?}"
        );
    }

    #[test]
    fn tokens_count_only_when_both_halves_are_reported() {
        assert_eq!(reported_tokens(Some(11), Some(4)), Some(15));
        assert_eq!(reported_tokens(Some(0), Some(0)), Some(0));
        assert_eq!(reported_tokens(Some(11), None), None);
        assert_eq!(reported_tokens(None, Some(4)), None);
        assert_eq!(reported_tokens(None, None), None);
    }

    #[test]
    fn cardinality_limit_caps_runaway_label() {
        let limit = CardinalityLimit::new(NonZeroUsize::new(2).unwrap());
        assert_eq!(limit.bucket("model", "a"), Label::Kept("a".to_string()));
        assert_eq!(limit.bucket("model", "b"), Label::Kept("b".to_string()));
        assert_eq!(limit.bucket("model", "a"), Label::Kept("a".to_string()));
        assert_eq!(limit.bucket("model", "c"), Label::Overflow);
        assert_eq!(limit.bucket("model", "d"), Label::Overflow);
        assert_eq!(limit.bucket("outcome", "ok"), Label::Kept("ok".to_string()));
    }
}
