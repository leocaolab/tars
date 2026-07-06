//! `serde_json::Value` ↔ `aws_smithy_types::Document` conversion.
//!
//! Doc 31 §5/C1 flags this as one of the two hard bits of M0: Bedrock's
//! tool input schemas (`ToolInputSchema::Json`) and tool-use arguments
//! (`ToolUseBlock::input`) are typed as the protocol-agnostic
//! [`Document`] rather than raw JSON. tars carries JSON as
//! [`serde_json::Value`] everywhere (tool schemas, tool-call args), so
//! we translate at the Bedrock boundary — in both directions:
//!
//! - **outbound** ([`value_to_document`]): a `ToolSpec.input_schema.schema`
//!   or an assistant-replay `ToolCall.arguments` → `Document` to hand the SDK.
//! - **inbound** ([`document_to_value`]): a model's `ToolUseBlock.input`
//!   `Document` → `Value` to hand back as `ChatEvent::ToolCallEnd.parsed_args`
//!   (whose invariant is a parsed JSON object).
//!
//! The mapping is total and lossless for the JSON data model. The one
//! representational choice is numbers: `Document::Number` distinguishes
//! `PosInt(u64)` / `NegInt(i64)` / `Float(f64)`, so we pick the tightest
//! variant serde already resolved (`as_u64` → `as_i64` → `as_f64`),
//! mirroring how `serde_json::Number` itself is stored.

use std::collections::HashMap;

use aws_smithy_types::{Document, Number};
use serde_json::Value;

/// Convert a [`serde_json::Value`] into an [`aws_smithy_types::Document`].
///
/// Total over the JSON data model. A `serde_json::Number` that fits none
/// of `u64`/`i64`/`f64` cannot occur (serde constructs numbers from
/// exactly those), but if `as_f64` ever returned `None` we fall back to
/// `0.0` rather than panic — a number that serde produced but can't read
/// back is a serde-internal impossibility, not caller data to lose.
pub fn value_to_document(v: &Value) -> Document {
    match v {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => {
            let num = if let Some(u) = n.as_u64() {
                Number::PosInt(u)
            } else if let Some(i) = n.as_i64() {
                Number::NegInt(i)
            } else {
                Number::Float(n.as_f64().unwrap_or(0.0))
            };
            Document::Number(num)
        }
        Value::String(s) => Document::String(s.clone()),
        Value::Array(arr) => Document::Array(arr.iter().map(value_to_document).collect()),
        Value::Object(map) => Document::Object(
            map.iter()
                .map(|(k, val)| (k.clone(), value_to_document(val)))
                .collect::<HashMap<_, _>>(),
        ),
    }
}

/// Convert an [`aws_smithy_types::Document`] back into a
/// [`serde_json::Value`]. Inverse of [`value_to_document`]; used to
/// surface a model's tool-use `input` as canonical parsed JSON.
pub fn document_to_value(d: &Document) -> Value {
    match d {
        Document::Null => Value::Null,
        Document::Bool(b) => Value::Bool(*b),
        Document::Number(n) => match n {
            Number::PosInt(u) => Value::from(*u),
            Number::NegInt(i) => Value::from(*i),
            Number::Float(f) => {
                // serde_json cannot represent NaN/Inf; carry those as Null
                // rather than fabricate a finite stand-in.
                serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
            }
        },
        Document::String(s) => Value::String(s.clone()),
        Document::Array(arr) => Value::Array(arr.iter().map(document_to_value).collect()),
        Document::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), document_to_value(v))).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_nested_json() {
        let v = json!({
            "type": "object",
            "properties": {
                "q": { "type": "string" },
                "n": 42,
                "neg": -7,
                "ratio": 1.5,
                "flag": true,
                "opt": null,
                "tags": ["a", "b"]
            },
            "required": ["q"]
        });
        let doc = value_to_document(&v);
        let back = document_to_value(&doc);
        assert_eq!(back, v, "Value → Document → Value must be lossless");
    }

    #[test]
    fn number_variants_pick_tightest() {
        assert!(matches!(
            value_to_document(&json!(42u64)),
            Document::Number(Number::PosInt(42))
        ));
        assert!(matches!(
            value_to_document(&json!(-7i64)),
            Document::Number(Number::NegInt(-7))
        ));
        assert!(matches!(
            value_to_document(&json!(1.5f64)),
            Document::Number(Number::Float(_))
        ));
    }
}
