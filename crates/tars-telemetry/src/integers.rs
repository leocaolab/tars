//! Exports every `tracing` integer as an OpenTelemetry integer: the one rule both span attributes
//! and span events go through. tracing-opentelemetry's span attribute recording has no
//! `record_u64`, `record_i128` or `record_u128`, so those fields reach it through `record_debug`
//! and leave as text; [`SpanIntegers`] runs after it and records them again by this rule.
//!
//! It does not decide which spans exist, their names, status or other attributes: those are
//! tracing-opentelemetry's.

use opentelemetry::{Key, KeyValue, Value};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_opentelemetry::OtelData;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Returns `n` as OpenTelemetry's one integer type, `i64`, or as its decimal text when it does
/// not fit: a clamped `i64::MAX` would read as a real count, while the text keeps the value.
pub(crate) fn otel_integer<N>(n: N) -> Value
where
    N: TryInto<i64> + ToString + Copy,
{
    n.try_into()
        .map_or_else(|_| Value::from(n.to_string()), Value::I64)
}

/// Records a span's `u64`, `i128` and `u128` fields over the text tracing-opentelemetry recorded
/// for them. It must be layered after tracing-opentelemetry, which creates the span's attributes.
pub(crate) struct SpanIntegers;

impl<S> Layer<S> for SpanIntegers
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut integers = Integers::default();
        attrs.record(&mut integers);
        replace(integers.0, id, &ctx);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let mut integers = Integers::default();
        values.record(&mut integers);
        replace(integers.0, id, &ctx);
    }
}

/// Replaces, for each integer, the value of the span's last attribute under its key, which is the
/// one tracing-opentelemetry recorded for that field just before. A key with no attribute is a
/// field tracing-opentelemetry took for the span itself (`otel.name`, `otel.status_code`) and is
/// left to it.
fn replace<S>(integers: Vec<KeyValue>, id: &Id, ctx: &Context<'_, S>)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    if integers.is_empty() {
        return;
    }
    let Some(span) = ctx.span(id) else {
        return;
    };
    let mut extensions = span.extensions_mut();
    let Some(attributes) = extensions
        .get_mut::<OtelData>()
        .and_then(|data| data.builder.attributes.as_mut())
    else {
        return;
    };
    for integer in integers {
        if let Some(recorded) = attributes.iter_mut().rev().find(|kv| kv.key == integer.key) {
            recorded.value = integer.value;
        }
    }
}

/// The `u64`, `i128` and `u128` fields of one span record, by [`otel_integer`]; every other field
/// tracing-opentelemetry records as it is.
#[derive(Default)]
struct Integers(Vec<KeyValue>);

impl Visit for Integers {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.push(KeyValue::new(
            Key::from_static_str(field.name()),
            otel_integer(value),
        ));
    }
    fn record_i128(&mut self, field: &Field, value: i128) {
        self.0.push(KeyValue::new(
            Key::from_static_str(field.name()),
            otel_integer(value),
        ));
    }
    fn record_u128(&mut self, field: &Field, value: u128) {
        self.0.push(KeyValue::new(
            Key::from_static_str(field.name()),
            otel_integer(value),
        ));
    }
    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_u64_up_to_i64_max_is_an_integer() {
        assert_eq!(otel_integer(0_u64), Value::I64(0));
        assert_eq!(otel_integer(i64::MAX as u64), Value::I64(i64::MAX));
    }

    #[test]
    fn a_u64_past_i64_max_keeps_its_value_as_text() {
        assert_eq!(
            otel_integer(i64::MAX as u64 + 1),
            Value::from("9223372036854775808".to_owned())
        );
        assert_eq!(
            otel_integer(u64::MAX),
            Value::from("18446744073709551615".to_owned())
        );
    }

    #[test]
    fn a_128_bit_integer_in_i64_range_is_an_integer() {
        assert_eq!(otel_integer(i128::from(i64::MIN)), Value::I64(i64::MIN));
        assert_eq!(
            otel_integer(i128::from(i64::MIN) - 1),
            Value::from("-9223372036854775809".to_owned())
        );
        assert_eq!(otel_integer(7_u128), Value::I64(7));
    }
}
