//! Records each `tracing` event inside an exported span as one of that span's OpenTelemetry
//! events. tars names an event by its `event` field and counts in `u64`; tracing-opentelemetry's
//! own recording names an event by its `message` and has no `record_u64`, so its span events came
//! out unnamed with their counts as strings, and it is kept to spans. Integers go through
//! [`otel_integer`], the rule span attributes follow too.
//!
//! It does not decide which spans exist or what they carry: that is tracing-opentelemetry's, and
//! an event outside a span it exports is dropped here.

use std::borrow::Cow;
use std::time::SystemTime;

use opentelemetry::trace::{Event as SpanEvent, Status};
use opentelemetry::{KeyValue, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_log::NormalizeEvent as _;
use tracing_opentelemetry::OtelData;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::integers::otel_integer;

pub(crate) struct SpanEvents;

impl<S> Layer<S> for SpanEvents
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let span = match event.parent() {
            Some(id) => ctx.span(id),
            None if event.is_contextual() => ctx.lookup_current(),
            None => None,
        };
        let Some(span) = span else {
            return;
        };

        let normalized = event.normalized_metadata();
        let from_log = normalized.is_some();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        let mut fields = EventFields::new(from_log);
        event.record(&mut fields);
        let level = *meta.level();
        let error = fields.error.clone();
        let span_event = fields.into_span_event(meta);

        let mut extensions = span.extensions_mut();
        let Some(data) = extensions.get_mut::<OtelData>() else {
            return;
        };
        let builder = &mut data.builder;
        if let Some(status) = status_after(level, error, &builder.status) {
            builder.status = status;
        }
        builder.events.get_or_insert_with(Vec::new).push(span_event);
    }
}

/// The fields of one `tracing` event, sorted into what names the span event, what fails the
/// span, and the attributes.
struct EventFields {
    event: Option<String>,
    message: Option<String>,
    error: Option<String>,
    attributes: Vec<KeyValue>,
    /// Whether the event came from the `log` crate, whose `log.*` fields repeat its metadata.
    from_log: bool,
}

impl EventFields {
    fn new(from_log: bool) -> Self {
        Self {
            event: None,
            message: None,
            error: None,
            attributes: Vec::new(),
            from_log,
        }
    }

    fn record_value(&mut self, field: &Field, value: Value) {
        match field.name() {
            "event" => self.event = Some(value.as_str().into_owned()),
            "message" => self.message = Some(value.as_str().into_owned()),
            "error" => self.error = Some(value.as_str().into_owned()),
            name if self.from_log && name.starts_with("log.") => {}
            name => self.attributes.push(KeyValue::new(name, value)),
        }
    }

    fn into_span_event(self, meta: &Metadata<'_>) -> SpanEvent {
        let (name, message) = event_name(self.event, self.message, meta.name());
        let mut attributes = vec![
            KeyValue::new("level", meta.level().as_str()),
            KeyValue::new("target", meta.target().to_owned()),
        ];
        attributes.extend(message.map(|m| KeyValue::new("message", m)));
        attributes.extend(self.error.map(|e| KeyValue::new("error", e)));
        attributes.extend(self.attributes);
        attributes.extend(
            meta.file()
                .map(|f| KeyValue::new("code.filepath", f.to_owned())),
        );
        attributes.extend(
            meta.module_path()
                .map(|m| KeyValue::new("code.namespace", m.to_owned())),
        );
        attributes.extend(
            meta.line()
                .map(|l| KeyValue::new("code.lineno", i64::from(l))),
        );
        SpanEvent::new(name, SystemTime::now(), attributes, 0)
    }
}

impl Visit for EventFields {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record_value(field, Value::F64(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_value(field, Value::I64(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_value(field, otel_integer(value));
    }
    fn record_i128(&mut self, field: &Field, value: i128) {
        self.record_value(field, otel_integer(value));
    }
    fn record_u128(&mut self, field: &Field, value: u128) {
        self.record_value(field, otel_integer(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_value(field, Value::Bool(value));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_value(field, Value::from(value.to_owned()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_value(field, Value::from(format!("{value:?}")));
    }
}

/// Returns the span event's name and the message left over as an attribute. The name is the
/// event's `event` field, which every tars event carries; an event without one is named by its
/// message, and one with neither by its callsite, as `tracing` names it.
fn event_name(
    event: Option<String>,
    message: Option<String>,
    callsite: &'static str,
) -> (Cow<'static, str>, Option<String>) {
    match (event, message) {
        (Some(event), message) => (event.into(), message),
        (None, Some(message)) => (message.into(), None),
        (None, None) => (callsite.into(), None),
    }
}

/// Returns the span's new status after one of its events, or `None` to leave it. An event with an
/// `error` field fails the span with that text, and an ERROR event fails a span with no status
/// yet, as tracing-opentelemetry did before this layer took its events over.
fn status_after(level: Level, error: Option<String>, current: &Status) -> Option<Status> {
    match error {
        Some(error) => Some(Status::error(error)),
        None => (level == Level::ERROR && *current == Status::Unset).then(|| Status::error("")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_field_names_the_event_and_the_message_stays_an_attribute() {
        assert_eq!(
            event_name(
                Some("llm.call.finished".into()),
                Some("done".into()),
                "event src/x.rs:1"
            ),
            ("llm.call.finished".into(), Some("done".to_owned()))
        );
    }

    #[test]
    fn without_an_event_field_the_message_names_the_event() {
        assert_eq!(
            event_name(None, Some("done".into()), "event src/x.rs:1"),
            ("done".into(), None)
        );
    }

    #[test]
    fn with_neither_the_callsite_names_the_event() {
        assert_eq!(
            event_name(None, None, "event src/x.rs:1"),
            ("event src/x.rs:1".into(), None)
        );
    }

    #[test]
    fn an_error_field_fails_the_span_with_its_text() {
        assert_eq!(
            status_after(Level::WARN, Some("boom".into()), &Status::Ok),
            Some(Status::error("boom"))
        );
    }

    #[test]
    fn an_error_event_fails_only_a_span_without_a_status() {
        assert_eq!(
            status_after(Level::ERROR, None, &Status::Unset),
            Some(Status::error(""))
        );
        assert_eq!(status_after(Level::ERROR, None, &Status::Ok), None);
        assert_eq!(status_after(Level::WARN, None, &Status::Unset), None);
    }
}
