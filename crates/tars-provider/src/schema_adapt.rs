//! Schema-dialect adaptation for structured-output decoders.
//!
//! Consumers pass a plain, standard JSON Schema (draft-07-ish). Any
//! `$ref` into the schema's root `$defs`/`definitions` bag is resolved
//! by [`adapt_schema`] itself — structured-output decoders (gemini
//! `responseSchema`, vLLM `guided_json`) do NOT resolve refs, so an
//! unresolved `$ref` would silently strip every constraint it points at.
//! Each provider adapter then translates the inlined schema into its own
//! API dialect *inside* tars via [`adapt_schema`], so callers never
//! hand-craft per-provider schema variants — nor pre-inline refs.
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

/// Why a schema could not be adapted. A closed, typed set — the caller
/// branches on the case, and the real unresolved pointer rides verbatim
/// inside the variant (never a `parse_failed`-style sentinel). Both
/// variants mean the same thing to a decoder: a `$ref` that cannot be
/// turned into concrete constraints, which we refuse to send rather than
/// silently drop.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaAdaptError {
    /// A `$ref` named no definition in the root `$defs`/`definitions` bag
    /// (or used a pointer form we don't resolve). Inlining it is
    /// impossible; leaving it in place would send an unresolved ref to a
    /// decoder that ignores it. `pointer` is the exact `$ref` string.
    #[error("$ref {pointer:?} points at a missing definition; known defs: {known:?}")]
    DanglingRef { pointer: String, known: Vec<String> },
    /// A `$ref` closes a cycle — a recursive schema. It cannot be inlined
    /// (expansion never terminates), so adaptation refuses it rather than
    /// loop or emit a partial shape. `pointer` is the ref that closed the
    /// cycle.
    #[error("$ref {pointer:?} is recursive; a $ref cycle cannot be inlined")]
    RefCycle { pointer: String },
}

/// Adapt a standard (draft-07-ish) JSON Schema into `dialect`'s
/// structured-output decoder format. Returns an owned, adapted copy; the
/// input is not mutated.
///
/// Any `$ref` into the root `$defs`/`definitions` bag is inlined FIRST
/// (decoders don't resolve refs), then the dialect transform runs on the
/// fully-inlined schema — so constraints hidden behind a ref (e.g. an
/// `allOf`-wrapped enum inside a definition) still participate.
///
/// # Errors
/// A `$ref` that names no definition ([`SchemaAdaptError::DanglingRef`])
/// or forms a cycle ([`SchemaAdaptError::RefCycle`]) is returned as `Err`
/// carrying the unresolved pointer. Such a ref is NEVER silently dropped,
/// defaulted, or passed through — the caller learns the exact pointer.
pub fn adapt_schema(schema: &Value, dialect: SchemaDialect) -> Result<Value, SchemaAdaptError> {
    let mut out = schema.clone();
    inline_refs(&mut out)?;
    adapt_node(&mut out, dialect);
    Ok(out)
}

/// Resolve every `$ref` in `value` against its root `$defs`/`definitions`
/// bag, inline the target, then drop the bag. Runs for ALL dialects: the
/// precondition "refs are already inlined" is gone — tars owns it now.
/// A dangling or cyclic ref is a hard `Err` (carrying the pointer), never
/// a silent pass-through of an unresolved ref.
fn inline_refs(value: &mut Value) -> Result<(), SchemaAdaptError> {
    // The definitions bag is `$defs` (2020-12) or `definitions` (draft-07,
    // what the pinned schemars emits). Take ownership once at the root so
    // we can drop it after resolution.
    let bag = ["$defs", "definitions"].into_iter().find_map(|k| match value.get(k) {
        Some(Value::Object(m)) => Some((k, m.clone())),
        _ => None,
    });
    let (key, defs) = match bag {
        Some((k, m)) => (Some(k), m),
        // No bag: there is nothing to inline INTO. But a stray `$ref` could
        // still appear — resolving against an empty bag turns it into a
        // DanglingRef error rather than letting it pass silently.
        None => (None, serde_json::Map::new()),
    };
    let mut active: Vec<String> = Vec::new();
    resolve_node(value, &defs, &mut active)?;
    if let Some(key) = key {
        if let Value::Object(m) = value {
            m.remove(key);
        }
    }
    Ok(())
}

/// Depth-first `$ref` resolution with cycle detection. `active` is the
/// stack of definition names currently being expanded on this path: a
/// `$ref` to a name already on the stack is a cycle (recursive schema),
/// which cannot be inlined. Diamond reuse (the same def referenced in
/// sibling branches) is fine — each expansion is pushed then popped.
fn resolve_node(
    node: &mut Value,
    defs: &serde_json::Map<String, Value>,
    active: &mut Vec<String>,
) -> Result<(), SchemaAdaptError> {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(ref_str)) = map.get("$ref").cloned() {
                let name = ref_str
                    .strip_prefix("#/$defs/")
                    .or_else(|| ref_str.strip_prefix("#/definitions/"));
                let name = match name {
                    Some(n) => n,
                    // An unrecognised pointer form (external URL, JSON
                    // Pointer we don't walk) can't be inlined here.
                    None => {
                        return Err(SchemaAdaptError::DanglingRef {
                            pointer: ref_str.clone(),
                            known: defs.keys().cloned().collect(),
                        });
                    }
                };
                if active.iter().any(|a| a == name) {
                    return Err(SchemaAdaptError::RefCycle { pointer: ref_str });
                }
                let target = match defs.get(name) {
                    Some(t) => t.clone(),
                    None => {
                        return Err(SchemaAdaptError::DanglingRef {
                            pointer: ref_str.clone(),
                            known: defs.keys().cloned().collect(),
                        });
                    }
                };
                // Replace the whole `{"$ref": ...}` node with the target,
                // then recurse into it (the target may itself carry refs).
                *node = target;
                active.push(name.to_string());
                resolve_node(node, defs, active)?;
                active.pop();
                return Ok(());
            }
            for v in map.values_mut() {
                resolve_node(v, defs, active)?;
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                resolve_node(item, defs, active)?;
            }
        }
        _ => {}
    }
    Ok(())
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
                        "additionalProperties", // boolean form left after the convert above
                        "title",
                        "description",
                        // `$defs`/`definitions`: `inline_refs` already dropped
                        // the ROOT bag for every dialect; these guard the (not
                        // schemars-emitted) nested-bag case for gemini. NO
                        // `$ref` here — a stray ref is now a hard error at
                        // inline time, never silently stripped.
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
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
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
        let out = adapt_schema(&raw, SchemaDialect::OpenAi).unwrap();
        assert!(out.get("allOf").is_none(), "allOf removed: {out}");
        assert_eq!(out["type"], json!("string"));
        assert_eq!(out["enum"], json!(["a"]));
        assert_eq!(out["description"], json!("x"), "sibling kept: {out}");
    }

    #[test]
    fn anyof_and_oneof_single_element_flattened() {
        for combinator in ["anyOf", "oneOf"] {
            let raw = json!({ combinator: [{"enum": ["x"], "type": "string"}] });
            let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
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
        let out = adapt_schema(&raw, SchemaDialect::OpenAi).unwrap();
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
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
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
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
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
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
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
        let out = adapt_schema(&raw, SchemaDialect::OpenAi).unwrap();
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
        assert_eq!(adapt_schema(&raw, SchemaDialect::Passthrough).unwrap(), raw);
        assert_eq!(adapt_schema(&raw, SchemaDialect::Vllm).unwrap(), raw);
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
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
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

    // ── $ref inlining (the defect this commit fixes) ──────────────────

    /// THE REGRESSION: a schemars-shaped schema whose array items are a
    /// `$ref` into `definitions`. Before the fix, the gemini branch
    /// STRIPPED `$ref`, emitting an item with NO `verdict`/`reply` — a
    /// structurally-valid but silently-wrong schema. Now the ref is
    /// inlined and the item carries its real properties.
    #[test]
    fn ref_into_definitions_is_inlined_not_stripped() {
        let raw = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {"$ref": "#/definitions/Item"}
                }
            },
            "definitions": {
                "Item": {
                    "type": "object",
                    "properties": {
                        "verdict": {"type": "string"},
                        "reply": {"type": "string"}
                    },
                    "required": ["verdict", "reply"]
                }
            }
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
        let item = &out["properties"]["items"]["items"];
        assert!(item.get("$ref").is_none(), "$ref resolved away: {item}");
        assert_eq!(item["properties"]["verdict"]["type"], json!("string"), "verdict kept: {item}");
        assert_eq!(item["properties"]["reply"]["type"], json!("string"), "reply kept: {item}");
        // The definitions bag is dropped after inlining.
        assert!(out.get("definitions").is_none(), "definitions dropped: {out}");
        assert!(!out.to_string().contains("$ref"), "no $ref survives: {out}");
    }

    /// A constraint hidden BEHIND a ref (allOf-wrapped enum inside a def)
    /// must be inlined BEFORE the flatten runs, so it still gets hoisted.
    #[test]
    fn ref_resolved_before_allof_flatten() {
        let raw = json!({
            "type": "object",
            "properties": {"status": {"$ref": "#/$defs/Status"}},
            "$defs": {
                "Status": {"allOf": [{"enum": ["open", "closed"], "type": "string"}]}
            }
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
        let status = &out["properties"]["status"];
        assert!(status.get("allOf").is_none(), "allOf flattened after inline: {status}");
        assert_eq!(status["enum"], json!(["open", "closed"]), "enum hoisted: {status}");
    }

    /// A `$ref` naming no definition is an `Err` carrying the exact
    /// pointer — never a silently-dropped node.
    #[test]
    fn dangling_ref_errors_with_pointer() {
        let raw = json!({
            "type": "object",
            "properties": {"x": {"$ref": "#/definitions/Missing"}},
            "definitions": {"Other": {"type": "string"}}
        });
        let err = adapt_schema(&raw, SchemaDialect::Gemini).unwrap_err();
        match err {
            SchemaAdaptError::DanglingRef { pointer, known } => {
                assert_eq!(pointer, "#/definitions/Missing");
                assert_eq!(known, vec!["Other".to_string()]);
            }
            other => panic!("expected DanglingRef, got {other:?}"),
        }
    }

    /// A `$ref` with no definitions bag at all is still a hard error, not
    /// a pass-through of an unresolved ref.
    #[test]
    fn ref_without_defs_bag_errors() {
        let raw = json!({"$ref": "#/definitions/Nope"});
        let err = adapt_schema(&raw, SchemaDialect::Gemini).unwrap_err();
        assert!(matches!(err, SchemaAdaptError::DanglingRef { .. }), "got {err:?}");
    }

    /// A recursive schema (`$ref` cycle) cannot be inlined — it is an
    /// `Err(RefCycle)` naming the ref that closed the cycle, never an
    /// infinite loop or a partial shape.
    #[test]
    fn recursive_ref_errors_as_cycle() {
        let raw = json!({
            "$ref": "#/definitions/Node",
            "definitions": {
                "Node": {
                    "type": "object",
                    "properties": {"child": {"$ref": "#/definitions/Node"}}
                }
            }
        });
        let err = adapt_schema(&raw, SchemaDialect::Gemini).unwrap_err();
        match err {
            SchemaAdaptError::RefCycle { pointer } => {
                assert_eq!(pointer, "#/definitions/Node");
            }
            other => panic!("expected RefCycle, got {other:?}"),
        }
    }

    /// Diamond reuse (the same def referenced in two sibling branches) is
    /// NOT a cycle — both inline independently.
    #[test]
    fn diamond_reuse_is_not_a_cycle() {
        let raw = json!({
            "type": "object",
            "properties": {
                "a": {"$ref": "#/definitions/Leaf"},
                "b": {"$ref": "#/definitions/Leaf"}
            },
            "definitions": {"Leaf": {"type": "string", "enum": ["x"]}}
        });
        let out = adapt_schema(&raw, SchemaDialect::OpenAi).unwrap();
        assert_eq!(out["properties"]["a"]["enum"], json!(["x"]));
        assert_eq!(out["properties"]["b"]["enum"], json!(["x"]));
    }

    /// Inlining runs for every dialect, including Passthrough/Vllm — the
    /// "refs already inlined" precondition is gone workspace-wide.
    #[test]
    fn refs_inlined_for_passthrough_and_vllm() {
        let raw = json!({
            "type": "object",
            "properties": {"x": {"$ref": "#/definitions/S"}},
            "definitions": {"S": {"type": "string"}}
        });
        for d in [SchemaDialect::Passthrough, SchemaDialect::Vllm] {
            let out = adapt_schema(&raw, d).unwrap();
            assert_eq!(out["properties"]["x"]["type"], json!("string"), "{d:?}: inlined");
            assert!(!out.to_string().contains("$ref"), "{d:?}: no $ref: {out}");
        }
    }
}
