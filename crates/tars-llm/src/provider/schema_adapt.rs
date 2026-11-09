//! Rewrites a standard (draft-07) JSON Schema into the dialect of one provider's
//! structured-output decoder, so a caller writes one schema for every provider.
//!
//! Every `$ref` into the root `$defs` / `definitions` is inlined first: the decoders (Gemini's
//! `responseSchema`, vLLM's `guided_json`) do not resolve refs, and would drop every constraint
//! behind one.
//!
//! Gemini does not enforce an `allOf`-wrapped enum, which is how schemars emits a closed enum
//! (`{"allOf":[{"enum":[...],"type":"string"}]}`); a weak model then emits values outside it, once
//! looping on `-01` thousands of times until `max_tokens`. A one-element `allOf`, `anyOf` or
//! `oneOf` is therefore flattened into its node, where Gemini enforces the enum.
//!
//! Gemini refuses `additionalProperties`, which is how a map (`{type: object,
//! additionalProperties: V}`) is written. Such a map is sent as an array of `{key, value}` entries
//! with `V` kept as it is, and [`restore_maps`] turns the reply back into the map the caller's
//! schema describes, so a caller never sees the array.

use serde_json::{Map, Value};

/// The most nodes a schema may have once its refs are inlined. A schema comes from a tool's
/// author or an MCP server, and a few hundred bytes of definitions that each refer to the next
/// twice inline to 2^n nodes.
const MAX_NODES: usize = 100_000;

/// The deepest a schema may nest once its refs are inlined; serde_json refuses to parse deeper
/// JSON, and a chain of definitions that each refer to the next would otherwise recurse once per
/// definition.
const MAX_DEPTH: usize = 128;

/// The dialect a schema is adapted into. `Vllm` and `Passthrough` change nothing beyond inlining
/// refs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaDialect {
    Gemini,
    OpenAi,
    Vllm,
    Passthrough,
}

/// A schema that cannot be adapted, refused rather than sent to a decoder that would ignore part
/// of it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaAdaptError {
    /// The `$ref` names no root definition, or is not of the form `#/$defs/<name>` or
    /// `#/definitions/<name>`.
    #[error("$ref {pointer:?} points at a missing definition; known defs: {known:?}")]
    DanglingRef { pointer: String, known: Vec<String> },
    /// The `$ref` closes a cycle, so inlining would never end.
    #[error("$ref {pointer:?} is recursive; a $ref cycle cannot be inlined")]
    RefCycle { pointer: String },
    /// Inlining the refs would produce more than `limit` nodes.
    #[error("the schema grows past {limit} nodes once its $refs are inlined")]
    TooLarge { limit: usize },
    /// The schema nests deeper than `limit` levels once its refs are inlined.
    #[error("the schema nests deeper than {limit} levels once its $refs are inlined")]
    TooDeep { limit: usize },
}

/// One step from a value to a part of it, as the caller's schema describes the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceStep {
    /// The named property of an object.
    Field(String),
    /// Every item of an array.
    Items,
    /// Every value of a map.
    MapValues,
}

/// Where in a value a map was sent as an array of `{key, value}` entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapPath(Vec<InstanceStep>);

/// A schema in a provider's dialect, and where it sent a map as an array.
#[derive(Debug, Clone, PartialEq)]
pub struct AdaptedSchema {
    pub schema: Value,
    /// Each map comes before the maps inside its values, so [`restore_maps`] meets the outer one
    /// first. Only the Gemini dialect encodes maps; for the others this is empty.
    pub encoded_maps: Vec<MapPath>,
}

/// Returns `schema` adapted to `dialect`. Refs are inlined before the dialect's rewrite, so a
/// constraint behind a ref (an `allOf`-wrapped enum in a definition) is rewritten too.
///
/// # Errors
///
/// [`SchemaAdaptError::DanglingRef`] or [`SchemaAdaptError::RefCycle`], carrying the `$ref`;
/// [`SchemaAdaptError::TooLarge`] or [`SchemaAdaptError::TooDeep`] when inlining would pass the
/// bounds.
pub fn adapt_schema(
    schema: &Value,
    dialect: SchemaDialect,
) -> Result<AdaptedSchema, SchemaAdaptError> {
    let mut out = schema.clone();
    inline_refs(&mut out)?;
    let mut encoded_maps = Vec::new();
    if !matches!(dialect, SchemaDialect::Vllm | SchemaDialect::Passthrough) {
        adapt_node(&mut out, dialect, &mut Vec::new(), &mut encoded_maps);
    }
    Ok(AdaptedSchema {
        schema: out,
        encoded_maps,
    })
}

/// Turns every array of `{key, value}` entries at `maps` in `value` back into a map. A part that
/// is not such an array is left as the provider sent it, for the caller's own parse to judge.
pub fn restore_maps(value: &mut Value, maps: &[MapPath]) {
    for MapPath(steps) in maps {
        restore_at(value, steps);
    }
}

/// Restores the maps at `maps` inside an object's fields, as [`restore_maps`] does for a value
/// that is this object. A map at the object itself cannot be one: the object is not an array.
pub fn restore_field_maps(object: &mut Map<String, Value>, maps: &[MapPath]) {
    for MapPath(steps) in maps {
        if let Some((InstanceStep::Field(name), rest)) = steps.split_first() {
            if let Some(v) = object.get_mut(name) {
                restore_at(v, rest);
            }
        }
    }
}

fn restore_at(value: &mut Value, steps: &[InstanceStep]) {
    let Some((step, rest)) = steps.split_first() else {
        if let Some(map) = entries_to_map(value) {
            *value = Value::Object(map);
        }
        return;
    };
    match (step, value) {
        (InstanceStep::Field(name), Value::Object(obj)) => {
            if let Some(v) = obj.get_mut(name) {
                restore_at(v, rest);
            }
        }
        (InstanceStep::Items, Value::Array(items)) => {
            for v in items {
                restore_at(v, rest);
            }
        }
        (InstanceStep::MapValues, Value::Object(obj)) => {
            for v in obj.values_mut() {
                restore_at(v, rest);
            }
        }
        _ => {}
    }
}

/// Returns the map an array of `{key, value}` entries spells, or `None` when `value` is not one.
fn entries_to_map(value: &Value) -> Option<Map<String, Value>> {
    let Value::Array(entries) = value else {
        return None;
    };
    entries
        .iter()
        .map(|entry| match entry {
            Value::Object(e) if e.len() == 2 => match (e.get("key"), e.get("value")) {
                (Some(Value::String(k)), Some(v)) => Some((k.clone(), v.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Replaces every `$ref` with its root definition, then removes the definitions.
fn inline_refs(value: &mut Value) -> Result<(), SchemaAdaptError> {
    // `$defs` is 2020-12; `definitions` is draft-07, which the pinned schemars emits.
    let bag = ["$defs", "definitions"]
        .into_iter()
        .find_map(|k| match value.get(k) {
            Some(Value::Object(m)) => Some((k, m.clone())),
            _ => None,
        });
    let (key, defs) = match bag {
        Some((k, m)) => (Some(k), m),
        // Resolving against no definitions still turns a stray `$ref` into an error.
        None => (None, Map::new()),
    };
    let mut walk = Walk {
        defs: &defs,
        active: Vec::new(),
        nodes: 0,
    };
    walk.resolve_node(value, 0)?;
    if let Some(key) = key {
        if let Value::Object(m) = value {
            m.remove(key);
        }
    }
    Ok(())
}

/// One inlining pass. `active` holds the definitions being expanded on this path: a ref to one of
/// them is a cycle, while the same definition in two sibling branches is not. `nodes` counts the
/// nodes visited so far, which is the size of the inlined schema.
struct Walk<'a> {
    defs: &'a Map<String, Value>,
    active: Vec<String>,
    nodes: usize,
}

impl Walk<'_> {
    fn resolve_node(&mut self, node: &mut Value, depth: usize) -> Result<(), SchemaAdaptError> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(SchemaAdaptError::TooLarge { limit: MAX_NODES });
        }
        if depth > MAX_DEPTH {
            return Err(SchemaAdaptError::TooDeep { limit: MAX_DEPTH });
        }
        match node {
            Value::Object(map) => {
                if let Some(Value::String(ref_str)) = map.get("$ref").cloned() {
                    let (name, target) = self.target(&ref_str)?;
                    if self.active.contains(&name) {
                        return Err(SchemaAdaptError::RefCycle { pointer: ref_str });
                    }
                    *node = target;
                    self.active.push(name);
                    // A ref expands in place, one level deeper, so a chain of refs is bounded by
                    // the same depth as nesting.
                    self.resolve_node(node, depth + 1)?;
                    self.active.pop();
                    return Ok(());
                }
                for v in map.values_mut() {
                    self.resolve_node(v, depth + 1)?;
                }
            }
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    self.resolve_node(item, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Returns the name and a copy of the root definition a `$ref` names.
    fn target(&self, ref_str: &str) -> Result<(String, Value), SchemaAdaptError> {
        ref_str
            .strip_prefix("#/$defs/")
            .or_else(|| ref_str.strip_prefix("#/definitions/"))
            .and_then(|name| self.defs.get(name).map(|t| (name.to_string(), t.clone())))
            .ok_or_else(|| SchemaAdaptError::DanglingRef {
                pointer: ref_str.to_string(),
                known: self.defs.keys().cloned().collect(),
            })
    }
}

/// Rewrites one schema node and its children for `dialect`. `at` is where the node's value sits
/// in the caller's value; each map the Gemini rewrite sends as an array is pushed to `maps`.
fn adapt_node(
    node: &mut Value,
    dialect: SchemaDialect,
    at: &mut Vec<InstanceStep>,
    maps: &mut Vec<MapPath>,
) {
    match node {
        Value::Object(map) => {
            // ── A. One-element allOf / anyOf / oneOf.
            //
            // Runs before the dialect rewrite, so the hoisted `enum` and `type` go through it. One
            // member adds nothing the node cannot say itself; its keys win over the node's. A
            // combinator of several members is a real union or intersection and stays.
            for combinator in ["allOf", "anyOf", "oneOf"] {
                let flattenable = matches!(
                    map.get(combinator),
                    Some(Value::Array(arr))
                        if arr.len() == 1 && matches!(arr.first(), Some(Value::Object(_)))
                );
                if flattenable {
                    if let Some(Value::Array(arr)) = map.remove(combinator) {
                        if let Some(Value::Object(inner)) = arr.into_iter().next() {
                            for (k, v) in inner {
                                map.insert(k, v);
                            }
                        }
                    }
                }
            }

            match dialect {
                SchemaDialect::Gemini => {
                    if let Some(entries) = map_as_entries(map, at, maps) {
                        *node = entries;
                        return;
                    }
                    // ── B.2 drop what Gemini's OpenAPI 3.0 dialect refuses.
                    for k in [
                        "$schema",
                        "$id",
                        // The boolean form, and the map values of an object that also names
                        // properties: the constraint on the extra keys is lost.
                        "additionalProperties",
                        "title",
                        "description",
                        // Nested definitions; `inline_refs` removed the root ones, and a `$ref`
                        // never reaches here.
                        "$defs",
                        "definitions",
                        "$comment",
                        "examples",
                        "default",
                        "const", // dropped, not turned into a one-value enum: the constraint is lost
                    ] {
                        map.remove(k);
                    }
                    // ── B.3 OpenAPI says `nullable: true`, not `"type": ["string", "null"]`.
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
                    // Strict mode requires `additionalProperties: false` on every object.
                    if matches!(map.get("type"), Some(Value::String(t)) if t == "object")
                        && !map.contains_key("additionalProperties")
                    {
                        map.insert("additionalProperties".into(), Value::Bool(false));
                    }
                }
                SchemaDialect::Vllm | SchemaDialect::Passthrough => {}
            }

            for (key, v) in map.iter_mut() {
                match (key.as_str(), v) {
                    // The keys of `properties` are the caller's field names, not keywords, so a
                    // field called `description` or `title` is kept.
                    ("properties", Value::Object(props)) => {
                        for (name, s) in props.iter_mut() {
                            at.push(InstanceStep::Field(name.clone()));
                            adapt_node(s, dialect, at, maps);
                            at.pop();
                        }
                    }
                    ("items", s) => {
                        at.push(InstanceStep::Items);
                        adapt_node(s, dialect, at, maps);
                        at.pop();
                    }
                    ("additionalProperties", s) => {
                        at.push(InstanceStep::MapValues);
                        adapt_node(s, dialect, at, maps);
                        at.pop();
                    }
                    (_, s) => adapt_node(s, dialect, at, maps),
                }
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                adapt_node(v, dialect, at, maps);
            }
        }
        _ => {}
    }
}

/// Returns the array-of-entries schema of a map node (`{type: object, additionalProperties: V}`
/// with no named properties) and records where it sits; `None` for any other node. `V` is adapted in place and keeps its own constraints.
fn map_as_entries(
    map: &mut Map<String, Value>,
    at: &mut Vec<InstanceStep>,
    maps: &mut Vec<MapPath>,
) -> Option<Value> {
    let is_map = map.get("type") == Some(&Value::String("object".into()))
        && matches!(map.get("additionalProperties"), Some(Value::Object(_)))
        && !matches!(map.get("properties"), Some(Value::Object(p)) if !p.is_empty());
    if !is_map {
        return None;
    }
    let mut value_schema = map.remove("additionalProperties")?;
    maps.push(MapPath(at.clone()));
    at.push(InstanceStep::MapValues);
    adapt_node(&mut value_schema, SchemaDialect::Gemini, at, maps);
    at.pop();
    Some(serde_json::json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "value": value_schema,
            },
            "required": ["key", "value"],
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn allof_single_element_is_flattened() {
        let raw = json!({
            "allOf": [{"enum": ["open", "resolved"], "type": "string"}]
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap().schema;
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
        // OpenAi, because Gemini's rewrite drops `description`.
        let raw = json!({
            "allOf": [{"enum": ["a"], "type": "string"}],
            "description": "x"
        });
        let out = adapt_schema(&raw, SchemaDialect::OpenAi).unwrap().schema;
        assert!(out.get("allOf").is_none(), "allOf removed: {out}");
        assert_eq!(out["type"], json!("string"));
        assert_eq!(out["enum"], json!(["a"]));
        assert_eq!(out["description"], json!("x"), "sibling kept: {out}");
    }

    #[test]
    fn anyof_and_oneof_single_element_flattened() {
        for combinator in ["anyOf", "oneOf"] {
            let raw = json!({ combinator: [{"enum": ["x"], "type": "string"}] });
            let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap().schema;
            assert!(out.get(combinator).is_none(), "{combinator} removed: {out}");
            assert_eq!(out["enum"], json!(["x"]), "{combinator} hoisted: {out}");
        }
    }

    #[test]
    fn multi_element_allof_left_untouched() {
        let raw = json!({
            "allOf": [{"type": "string"}, {"minLength": 1}]
        });
        let out = adapt_schema(&raw, SchemaDialect::OpenAi).unwrap().schema;
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
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap().schema;
        let s = out.to_string();
        assert!(!s.contains("$schema"), "stripped $schema: {s}");
        assert!(!s.contains("$id"), "stripped $id: {s}");
        assert!(
            !s.contains("additionalProperties"),
            "stripped additionalProperties: {s}"
        );
        assert!(!s.contains("title"), "stripped title: {s}");
        assert!(!s.contains("description"), "stripped description: {s}");
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
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap().schema;
        assert_eq!(out["type"], json!("array"), "converted to array: {out}");
        assert!(out["items"].is_object(), "items present: {out}");
        let props = &out["items"]["properties"];
        assert_eq!(
            props["key"]["type"],
            json!("string"),
            "key is a string: {out}"
        );
        assert!(
            props["value"]["properties"].get("x").is_some(),
            "original property kept: {out}"
        );
        assert!(
            !out.to_string().contains("additionalProperties"),
            "no additionalProperties: {out}"
        );
    }

    #[test]
    fn gemini_nullable() {
        let raw = json!({"type": ["string", "null"]});
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap().schema;
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
        let out = adapt_schema(&raw, SchemaDialect::OpenAi).unwrap().schema;
        assert_eq!(out["additionalProperties"], json!(false));
        assert_eq!(
            out["properties"]["nested"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn passthrough_and_vllm_unchanged() {
        let raw = json!({
            "allOf": [{"enum": ["open"], "type": "string"}],
            "$schema": "draft-07"
        });
        assert_eq!(
            adapt_schema(&raw, SchemaDialect::Passthrough)
                .unwrap()
                .schema,
            raw
        );
        assert_eq!(adapt_schema(&raw, SchemaDialect::Vllm).unwrap().schema, raw);
    }

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
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap().schema;
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

    // ── $ref inlining ─────────────────────────────────────────────────

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
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap().schema;
        let item = &out["properties"]["items"]["items"];
        assert!(item.get("$ref").is_none(), "$ref resolved away: {item}");
        assert_eq!(
            item["properties"]["verdict"]["type"],
            json!("string"),
            "verdict kept: {item}"
        );
        assert_eq!(
            item["properties"]["reply"]["type"],
            json!("string"),
            "reply kept: {item}"
        );
        assert!(
            out.get("definitions").is_none(),
            "definitions dropped: {out}"
        );
        assert!(!out.to_string().contains("$ref"), "no $ref survives: {out}");
    }

    #[test]
    fn ref_resolved_before_allof_flatten() {
        let raw = json!({
            "type": "object",
            "properties": {"status": {"$ref": "#/$defs/Status"}},
            "$defs": {
                "Status": {"allOf": [{"enum": ["open", "closed"], "type": "string"}]}
            }
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap().schema;
        let status = &out["properties"]["status"];
        assert!(
            status.get("allOf").is_none(),
            "allOf flattened after inline: {status}"
        );
        assert_eq!(
            status["enum"],
            json!(["open", "closed"]),
            "enum hoisted: {status}"
        );
    }

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

    #[test]
    fn ref_without_defs_bag_errors() {
        let raw = json!({"$ref": "#/definitions/Nope"});
        let err = adapt_schema(&raw, SchemaDialect::Gemini).unwrap_err();
        assert!(
            matches!(err, SchemaAdaptError::DanglingRef { .. }),
            "got {err:?}"
        );
    }

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
        let out = adapt_schema(&raw, SchemaDialect::OpenAi).unwrap().schema;
        assert_eq!(out["properties"]["a"]["enum"], json!(["x"]));
        assert_eq!(out["properties"]["b"]["enum"], json!(["x"]));
    }

    #[test]
    fn refs_inlined_for_passthrough_and_vllm() {
        let raw = json!({
            "type": "object",
            "properties": {"x": {"$ref": "#/definitions/S"}},
            "definitions": {"S": {"type": "string"}}
        });
        for d in [SchemaDialect::Passthrough, SchemaDialect::Vllm] {
            let out = adapt_schema(&raw, d).unwrap().schema;
            assert_eq!(
                out["properties"]["x"]["type"],
                json!("string"),
                "{d:?}: inlined"
            );
            assert!(!out.to_string().contains("$ref"), "{d:?}: no $ref: {out}");
        }
    }

    // ── maps under Gemini ─────────────────────────────────────────────

    fn scores_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "scores": {"type": "object", "additionalProperties": {"type": "integer"}}
            }
        })
    }

    #[test]
    fn gemini_map_of_scalars_becomes_key_value_entries_with_the_value_schema_kept() {
        let out = adapt_schema(&scores_schema(), SchemaDialect::Gemini).unwrap();
        let scores = &out.schema["properties"]["scores"];
        assert_eq!(scores["type"], json!("array"), "{scores}");
        assert_eq!(
            scores["items"]["properties"]["value"],
            json!({"type": "integer"})
        );
        assert!(
            !out.schema.to_string().contains("issue_id"),
            "no field a caller did not name: {}",
            out.schema
        );
        assert_eq!(
            out.encoded_maps,
            vec![MapPath(vec![InstanceStep::Field("scores".into())])]
        );
    }

    #[test]
    fn a_map_of_maps_records_the_outer_map_before_the_inner_one() {
        let raw = json!({
            "type": "object",
            "additionalProperties": {
                "type": "object",
                "additionalProperties": {"type": "string"}
            }
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
        assert_eq!(
            out.encoded_maps,
            vec![MapPath(vec![]), MapPath(vec![InstanceStep::MapValues])]
        );
        let inner = &out.schema["items"]["properties"]["value"];
        assert_eq!(
            inner["type"],
            json!("array"),
            "inner map encoded too: {inner}"
        );
    }

    #[test]
    fn restore_maps_turns_entries_back_into_the_callers_map() {
        let out = adapt_schema(&scores_schema(), SchemaDialect::Gemini).unwrap();
        let mut reply = json!({"scores": [{"key": "a", "value": 1}, {"key": "b", "value": 2}]});
        restore_maps(&mut reply, &out.encoded_maps);
        assert_eq!(reply, json!({"scores": {"a": 1, "b": 2}}));
    }

    #[test]
    fn restore_maps_restores_nested_maps_and_maps_inside_array_items() {
        let raw = json!({
            "type": "object",
            "properties": {
                "rows": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": {
                            "type": "object",
                            "additionalProperties": {"type": "integer"}
                        }
                    }
                }
            }
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
        let mut reply = json!({"rows": [
            [{"key": "r", "value": [{"key": "c", "value": 7}]}]
        ]});
        restore_maps(&mut reply, &out.encoded_maps);
        assert_eq!(reply, json!({"rows": [{"r": {"c": 7}}]}));
    }

    #[test]
    fn restore_maps_leaves_a_value_that_is_not_an_entry_array_as_sent() {
        let out = adapt_schema(&scores_schema(), SchemaDialect::Gemini).unwrap();
        let sent = json!({"scores": [{"key": "a", "value": 1, "extra": true}]});
        let mut reply = sent.clone();
        restore_maps(&mut reply, &out.encoded_maps);
        assert_eq!(reply, sent);
    }

    #[test]
    fn gemini_keeps_an_object_with_named_properties_and_drops_only_its_extra_keys_constraint() {
        let raw = json!({
            "type": "object",
            "properties": {"x": {"type": "string"}},
            "additionalProperties": {"type": "integer"}
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap();
        assert_eq!(out.schema["type"], json!("object"));
        assert_eq!(out.schema["properties"]["x"]["type"], json!("string"));
        assert!(out.encoded_maps.is_empty());
    }

    #[test]
    fn gemini_keeps_fields_named_like_the_keywords_it_strips() {
        let raw = json!({
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "description": {"type": "string"},
                "default": {"type": "boolean"}
            },
            "required": ["title", "description", "default"]
        });
        let out = adapt_schema(&raw, SchemaDialect::Gemini).unwrap().schema;
        for field in ["title", "description", "default"] {
            assert!(
                out["properties"].get(field).is_some(),
                "{field} kept: {out}"
            );
        }
    }

    // ── bounds ────────────────────────────────────────────────────────

    /// `d0 -> d1 -> ... -> dN`, each definition referring to the next twice.
    fn diamond_chain(n: usize) -> Value {
        let mut defs = Map::new();
        for i in 0..n {
            defs.insert(
                format!("d{i}"),
                json!({"type":"object","properties":{
                    "a":{"$ref":format!("#/definitions/d{}", i + 1)},
                    "b":{"$ref":format!("#/definitions/d{}", i + 1)}}}),
            );
        }
        defs.insert(format!("d{n}"), json!({"type":"string"}));
        json!({"$ref":"#/definitions/d0","definitions":defs})
    }

    #[test]
    fn a_schema_that_inlines_within_the_node_budget_is_adapted() {
        // 2^10 leaves, about 5 000 nodes.
        assert!(adapt_schema(&diamond_chain(10), SchemaDialect::Passthrough).is_ok());
    }

    #[test]
    fn a_schema_that_inlines_past_the_node_budget_is_refused() {
        // Under 2 KB of definitions, 2^18 leaves.
        let err = adapt_schema(&diamond_chain(18), SchemaDialect::Passthrough).unwrap_err();
        assert_eq!(err, SchemaAdaptError::TooLarge { limit: MAX_NODES });
    }

    /// `d0 -> d1 -> ... -> dN`, each definition only a ref to the next.
    fn ref_chain(n: usize) -> Value {
        let mut defs = Map::new();
        for i in 0..n {
            defs.insert(
                format!("d{i}"),
                json!({"$ref": format!("#/definitions/d{}", i + 1)}),
            );
        }
        defs.insert(format!("d{n}"), json!({"type":"string"}));
        json!({"$ref":"#/definitions/d0","definitions":defs})
    }

    #[test]
    fn a_ref_chain_within_the_depth_bound_is_inlined() {
        // The string under the last definition sits at depth `MAX_DEPTH`.
        let out = adapt_schema(&ref_chain(MAX_DEPTH - 2), SchemaDialect::Passthrough).unwrap();
        assert_eq!(out.schema, json!({"type": "string"}));
    }

    #[test]
    fn a_ref_chain_past_the_depth_bound_is_refused() {
        for n in [MAX_DEPTH - 1, 50_000] {
            let err = adapt_schema(&ref_chain(n), SchemaDialect::Passthrough).unwrap_err();
            assert_eq!(err, SchemaAdaptError::TooDeep { limit: MAX_DEPTH }, "n={n}");
        }
    }
}
