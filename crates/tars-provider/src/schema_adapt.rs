//! Schema-dialect adaptation for structured-output decoders.
//!
//! Consumers pass a plain, standard JSON Schema (draft-07-ish, with
//! `$ref`s already inlined). Each provider adapter translates it into
//! its own API dialect *inside* tars via [`adapt_schema`], so callers
//! never hand-craft per-provider schema variants.
//!
//! ## The bug this fixes
//!
//! Gemini's structured-output decoder does not enforce an `allOf`-wrapped
//! enum. schemars emits a closed enum (e.g. a `status` field) as
//! `{"allOf":[{"enum":[...],"type":"string"}]}`. Because the decoder
//! ignores the `allOf`, a weak model (gemini flash) is free to emit an
//! out-of-domain value — observed degenerating into an 8000× `-01`
//! repetition loop until `max_tokens` truncation. Flattening a
//! single-element `allOf` (`anyOf` / `oneOf` likewise) hoists the
//! `enum`/`type` to the node itself, where gemini *does* enforce it as a
//! hard constraint.
//!
//! The Gemini / OpenAI / Vllm transforms are ported from arc's
//! `crates/arc_core/src/schema.rs` `sanitize_node` (the source of truth
//! being migrated here), plus the new single-element `allOf` flatten.

use serde_json::Value;

/// The provider-specific structured-output dialect a schema is adapted
/// into. `Passthrough` (and `Vllm`, which is Passthrough in practice)
/// leave the schema untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaDialect {
    Gemini,
    OpenAi,
    Vllm,
    Passthrough,
}

/// Adapt a standard (draft-07-ish, `$ref`-already-inlined) JSON Schema
/// into `dialect`'s structured-output decoder format. Returns an owned,
/// adapted copy; the input is not mutated.
pub fn adapt_schema(schema: &Value, dialect: SchemaDialect) -> Value {
    let mut out = schema.clone();
    adapt_node(&mut out, dialect);
    out
}

fn adapt_node(node: &mut Value, dialect: SchemaDialect) {
    // Vllm and Passthrough are no-ops: the schema is sent verbatim.
    if matches!(dialect, SchemaDialect::Vllm | SchemaDialect::Passthrough) {
        return;
    }
    match node {
        Value::Object(map) => {
            // ── A. Single-element allOf/anyOf/oneOf flatten (the bug fix).
            //
            // Apply FIRST, before the dialect-specific rest, so the
            // hoisted `enum`/`type` participate in the subsequent
            // transforms. A combinator wrapping EXACTLY ONE object adds
            // no constraint a decoder can't express inline, and gemini
            // silently ignores the wrapper — so hoist the inner object's
            // keys (inner wins on conflict) and drop the combinator.
            // Multi-element combinators are left untouched (can't flatten
            // a real union/intersection safely).
            for combinator in ["allOf", "anyOf", "oneOf"] {
                // Only a one-element array of one object is flattenable.
                let flattenable = matches!(
                    map.get(combinator),
                    Some(Value::Array(arr))
                        if arr.len() == 1 && matches!(arr.first(), Some(Value::Object(_)))
                );
                if flattenable {
                    if let Some(Value::Array(arr)) = map.remove(combinator) {
                        if let Some(Value::Object(inner)) = arr.into_iter().next() {
                            // Inner wins on conflict (it carries the
                            // constraint); the node keeps any sibling key
                            // the inner doesn't define.
                            for (k, v) in inner {
                                map.insert(k, v);
                            }
                        }
                    }
                }
            }

            match dialect {
                SchemaDialect::Gemini => {
                    // ── B.1 map→array conversion.
                    //
                    // schemars emits `BTreeMap<String, T>` as
                    // `{type:object, additionalProperties:<TSchema>}`.
                    // Gemini's response_schema rejects
                    // `additionalProperties` entirely, so re-express the
                    // "map of N keyed items" as an array of items that
                    // each carry an explicit `issue_id` (the downstream
                    // array→dict re-wrap reads it back as the map key).
                    if map.get("type") == Some(&Value::String("object".into()))
                        && matches!(map.get("additionalProperties"), Some(Value::Object(_)))
                    {
                        if let Some(Value::Object(mut item_schema)) =
                            map.remove("additionalProperties")
                        {
                            let issue_id_schema = serde_json::json!({ "type": "string" });
                            if let Some(Value::Object(props)) = item_schema.get_mut("properties") {
                                props.insert("issue_id".into(), issue_id_schema);
                            } else {
                                item_schema.insert(
                                    "properties".into(),
                                    serde_json::json!({ "issue_id": issue_id_schema }),
                                );
                            }
                            map.clear();
                            map.insert("type".into(), Value::String("array".into()));
                            map.insert("items".into(), Value::Object(item_schema));
                        }
                    }
                    // ── B.2 strip meta + draft-07-only keywords gemini's
                    // OpenAPI-3.0 dialect rejects.
                    for k in [
                        "$schema",
                        "$id",
                        "$ref", // belt-and-suspenders — refs should be inlined already
                        "additionalProperties", // boolean form left after the convert above
                        "title",
                        "description",
                        "$defs",
                        "definitions",
                        "$comment",
                        "examples",
                        "default",
                        "const", // OpenAPI uses `enum` with one value, not `const`
                    ] {
                        map.remove(k);
                    }
                    // ── B.3 nullable conversion: OpenAPI uses
                    // `nullable:true`, not `"type":["string","null"]`.
                    if let Some(Value::Array(arr)) = map.get("type").cloned() {
                        let non_null: Vec<&Value> = arr
                            .iter()
                            .filter(|v| !matches!(v, Value::String(s) if s == "null"))
                            .collect();
                        if non_null.len() == 1 {
                            let nullable_present = arr
                                .iter()
                                .any(|v| matches!(v, Value::String(s) if s == "null"));
                            map.insert("type".into(), non_null[0].clone());
                            if nullable_present {
                                map.insert("nullable".into(), Value::Bool(true));
                            }
                        }
                    }
                }
                SchemaDialect::OpenAi => {
                    // OpenAI strict mode requires additionalProperties:false
                    // on every object schema. Add it where missing. (arc's
                    // OpenAI profile does ONLY this — no draft-07 strip.)
                    if matches!(map.get("type"), Some(Value::String(t)) if t == "object")
                        && !map.contains_key("additionalProperties")
                    {
                        map.insert("additionalProperties".into(), Value::Bool(false));
                    }
                }
                // Unreachable: Vllm/Passthrough returned at the top.
                SchemaDialect::Vllm | SchemaDialect::Passthrough => {}
            }

            for v in map.values_mut() {
                adapt_node(v, dialect);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                adapt_node(v, dialect);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// THE KEY TEST: a single-element `allOf` wrapping an enum is
    /// hoisted so the enum + type sit at the node itself — the form
    /// gemini's decoder actually enforces. No `allOf` survives.
    #[test]
    fn allof_single_element_is_flattened() {
        let raw = json!({
            "allOf": [{"enum": ["open", "resolved"], "type": "string"}]
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini);
        assert!(out.get("allOf").is_none(), "allOf removed: {out}");
        assert_eq!(out["type"], json!("string"), "type hoisted: {out}");
        assert_eq!(
            out["enum"],
            json!(["open", "resolved"]),
            "enum hoisted: {out}"
        );
    }

    #[test]
    fn allof_keeps_node_sibling_keys() {
        // `description` is a sibling the inner doesn't define — it should
        // survive the hoist (then Gemini's strip removes it, so test the
        // hoist itself via OpenAi, which doesn't strip description).
        let raw = json!({
            "allOf": [{"enum": ["a"], "type": "string"}],
            "description": "x"
        });
        let out = adapt_schema(&raw, SchemaDialect::OpenAi);
        assert!(out.get("allOf").is_none(), "allOf removed: {out}");
        assert_eq!(out["type"], json!("string"));
        assert_eq!(out["enum"], json!(["a"]));
        assert_eq!(out["description"], json!("x"), "sibling kept: {out}");
    }

    #[test]
    fn anyof_and_oneof_single_element_flattened() {
        for combinator in ["anyOf", "oneOf"] {
            let raw = json!({ combinator: [{"enum": ["x"], "type": "string"}] });
            let out = adapt_schema(&raw, SchemaDialect::Gemini);
            assert!(out.get(combinator).is_none(), "{combinator} removed: {out}");
            assert_eq!(out["enum"], json!(["x"]), "{combinator} hoisted: {out}");
        }
    }

    #[test]
    fn multi_element_allof_left_untouched() {
        // A real intersection (2+ members) cannot be flattened safely.
        let raw = json!({
            "allOf": [{"type": "string"}, {"minLength": 1}]
        });
        let out = adapt_schema(&raw, SchemaDialect::OpenAi);
        assert!(
            out.get("allOf").is_some(),
            "multi-element allOf preserved: {out}"
        );
        assert_eq!(out["allOf"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn gemini_strips_draft07_keywords() {
        let raw = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$id": "https://example.com/x",
            "title": "CriticResponse",
            "description": "top-level",
            "additionalProperties": false,
            "type": "object",
            "properties": {"x": {"type": "string"}}
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini);
        let s = out.to_string();
        assert!(!s.contains("$schema"), "stripped $schema: {s}");
        assert!(!s.contains("$id"), "stripped $id: {s}");
        assert!(
            !s.contains("additionalProperties"),
            "stripped additionalProperties: {s}"
        );
        assert!(!s.contains("title"), "stripped title: {s}");
        assert!(!s.contains("description"), "stripped description: {s}");
        // Structure preserved.
        assert!(s.contains("properties"), "preserved properties: {s}");
        assert_eq!(out["type"], json!("object"));
    }

    #[test]
    fn gemini_map_to_array() {
        let raw = json!({
            "type": "object",
            "additionalProperties": {
                "type": "object",
                "properties": {"x": {"type": "string"}}
            }
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini);
        assert_eq!(out["type"], json!("array"), "converted to array: {out}");
        assert!(out["items"].is_object(), "items present: {out}");
        let props = &out["items"]["properties"];
        assert_eq!(
            props["issue_id"]["type"],
            json!("string"),
            "issue_id injected: {out}"
        );
        assert!(props.get("x").is_some(), "original property kept: {out}");
        assert!(
            !out.to_string().contains("additionalProperties"),
            "no additionalProperties: {out}"
        );
    }

    #[test]
    fn gemini_nullable() {
        let raw = json!({"type": ["string", "null"]});
        let out = adapt_schema(&raw, SchemaDialect::Gemini);
        assert_eq!(out["type"], json!("string"), "single non-null type: {out}");
        assert_eq!(out["nullable"], json!(true), "nullable set: {out}");
    }

    #[test]
    fn openai_adds_additional_properties_false() {
        let raw = json!({
            "type": "object",
            "properties": {
                "nested": {"type": "object", "properties": {"x": {"type": "string"}}}
            }
        });
        let out = adapt_schema(&raw, SchemaDialect::OpenAi);
        assert_eq!(out["additionalProperties"], json!(false));
        assert_eq!(
            out["properties"]["nested"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn passthrough_and_vllm_unchanged() {
        // Only Gemini/OpenAi flatten — a schema with allOf passes through
        // Passthrough and Vllm completely unchanged.
        let raw = json!({
            "allOf": [{"enum": ["open"], "type": "string"}],
            "$schema": "draft-07"
        });
        assert_eq!(adapt_schema(&raw, SchemaDialect::Passthrough), raw);
        assert_eq!(adapt_schema(&raw, SchemaDialect::Vllm), raw);
    }

    /// Realistic nested case: a CriticResponse-shaped schema where the
    /// nested `status` field is an allOf-wrapped enum. After adaptation
    /// for Gemini, that nested status carries a flat top-level enum and
    /// no allOf — the constraint gemini now enforces.
    #[test]
    fn nested_critic_response_status_enum_flattened() {
        let raw = json!({
            "type": "object",
            "properties": {
                "findings": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "status": {
                                "allOf": [{
                                    "enum": ["open", "resolved", "acknowledged"],
                                    "type": "string"
                                }]
                            }
                        }
                    }
                }
            }
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini);
        let status = &out["properties"]["findings"]["items"]["properties"]["status"];
        assert!(
            status.get("allOf").is_none(),
            "nested allOf flattened: {status}"
        );
        assert_eq!(
            status["type"],
            json!("string"),
            "nested type hoisted: {status}"
        );
        assert_eq!(
            status["enum"],
            json!(["open", "resolved", "acknowledged"]),
            "nested enum hoisted: {status}"
        );
    }
}
