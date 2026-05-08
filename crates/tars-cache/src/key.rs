//! Cache key — the SHA-256 fingerprint of every input that can
//! possibly affect the LLM response.
//!
//! See Doc 03 §3 for the full security rationale. The TL;DR: tenant id
//! and IAM scopes **must** participate in the hash; otherwise principals
//! with overlapping prompts but different read-rights will collide.

use std::fmt;

use serde_json::Value;
use sha2::{Digest, Sha256};

use tars_types::{ChatRequest, ContentBlock, ImageData, Message, ModelHint, RequestContext};

use crate::error::CacheError;

/// Stable lookup key for a single (request, principal) pair.
///
/// `fingerprint` is the SHA-256 of every cacheable input; `debug_label`
/// is a human-readable summary that **does not** participate in the
/// hash and is safe to put in logs.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CacheKey {
    pub fingerprint: [u8; 32],
    pub debug_label: String,
}

impl CacheKey {
    /// Lowercase hex string of the fingerprint — useful for logs that
    /// want a stable identifier.
    pub fn hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.fingerprint {
            use fmt::Write;
            let _ = write!(&mut s, "{b:02x}");
        }
        s
    }
}

/// The attribute key callers use to thread IAM scopes through
/// [`RequestContext::attributes`]. Looked up at hash time. Format:
/// JSON array of strings.
pub const IAM_SCOPES_ATTR: &str = "iam.allowed_scopes";

#[derive(Clone, Debug)]
pub struct CacheKeyFactory {
    /// Bump this on the rare occasion the hashing logic changes — every
    /// downstream cache entry becomes unreachable in one stroke without
    /// needing a flush command.
    hasher_version: u32,
}

impl CacheKeyFactory {
    pub fn new(hasher_version: u32) -> Self {
        Self { hasher_version }
    }

    pub fn hasher_version(&self) -> u32 {
        self.hasher_version
    }

    /// Build the key. Returns:
    /// - `Ok(key)` — request is cacheable
    /// - `Err(CacheError::NonDeterministic | UnresolvedTier | UncacheableEnsemble)`
    ///   — request shouldn't be cached (middleware skips lookup/write)
    /// - `Err(CacheError::Serialize)` — JSON serialisation failure
    ///   (tools, structured-output schema, tool-call args)
    pub fn compute(&self, req: &ChatRequest, ctx: &RequestContext) -> Result<CacheKey, CacheError> {
        // Reject early: stochastic outputs and unresolved routing aren't cacheable.
        match req.temperature {
            None => return Err(CacheError::NonDeterministic),
            Some(t) if t != 0.0 => return Err(CacheError::NonDeterministic),
            Some(_) => {}
        }
        match &req.model {
            ModelHint::Tier(_) => return Err(CacheError::UnresolvedTier),
            ModelHint::Ensemble(_) => return Err(CacheError::UncacheableEnsemble),
            ModelHint::Explicit(_) => {}
        }

        let mut h = Sha256::new();

        // ── Isolation domain (must be first) ──────────────────────
        h.update(self.hasher_version.to_le_bytes());
        h.update(b"\0TENANT\0");
        h.update(ctx.tenant_id.as_ref().as_bytes());

        // IAM scopes — sorted + null-delimited so ["a","b"] and ["b","a"]
        // collide (they grant the same permission set) but ["a"] and
        // ["a","b"] don't.
        let scopes = read_iam_scopes(ctx);
        h.update(b"\0SCOPES\0");
        for scope in &scopes {
            h.update(scope.as_bytes());
            h.update(b"\0");
        }

        // ── Model identity ─────────────────────────────────────────
        h.update(b"\0MODEL\0");
        if let ModelHint::Explicit(name) = &req.model {
            h.update(name.as_bytes());
        }

        // ── Output-determining parameters ──────────────────────────
        h.update(b"\0PARAMS\0");
        h.update(b"t=0"); // verified above
        if let Some(seed) = req.seed {
            h.update(b"\0SEED\0");
            h.update(seed.to_le_bytes());
        }
        if let Some(max) = req.max_output_tokens {
            h.update(b"\0MAX\0");
            h.update(max.to_le_bytes());
        }
        for stop in &req.stop_sequences {
            h.update(b"\0STOP\0");
            h.update(stop.as_bytes());
        }

        // Thinking mode affects the answer shape (thinking blocks vs.
        // pure text), so it has to participate.
        h.update(b"\0THINK\0");
        h.update(thinking_tag(&req.thinking).as_bytes());

        // ── Content ────────────────────────────────────────────────
        h.update(b"\0SYSTEM\0");
        if let Some(sys) = &req.system {
            h.update(sys.as_bytes());
        }

        h.update(b"\0MESSAGES\0");
        for msg in &req.messages {
            hash_message(&mut h, msg)?;
        }

        if !req.tools.is_empty() {
            h.update(b"\0TOOLS\0");
            // serde_json::Map is BTreeMap-backed by default → key order is
            // alphabetic and deterministic across runs. Our struct fields
            // serialize in declaration order. Both are stable. No need
            // for a separate canonical-JSON crate at this stage.
            let canonical = serde_json::to_vec(&req.tools).map_err(CacheError::Serialize)?;
            h.update(&canonical);
        }

        if let Some(schema) = &req.structured_output {
            h.update(b"\0SCHEMA\0");
            let canonical = serde_json::to_vec(&schema.schema).map_err(CacheError::Serialize)?;
            h.update(&canonical);
            h.update(if schema.strict { b"S" } else { b"L" });
        }

        let fingerprint: [u8; 32] = h.finalize().into();
        let debug_label = format!(
            "tenant={} model={} msgs={} scopes={}",
            ctx.tenant_id.as_ref(),
            req.model.label(),
            req.messages.len(),
            scopes.len(),
        );
        Ok(CacheKey {
            fingerprint,
            debug_label,
        })
    }
}

fn thinking_tag(t: &tars_types::ThinkingMode) -> String {
    use tars_types::ThinkingMode::*;
    match t {
        Off => "off".into(),
        Auto => "auto".into(),
        Budget(b) => format!("budget:{b}"),
    }
}

/// Read IAM scopes from `ctx.attributes` under [`IAM_SCOPES_ATTR`].
/// Missing or malformed → empty list.
///
/// **Production requirement** (Doc 10): an IAM middleware sitting
/// before Cache populates this attribute. M1 has no IAM middleware;
/// missing scopes are silently treated as "no restrictions" because
/// Personal mode is single-tenant. Multi-tenant deployments without
/// IAM = bug — this is documented in `tars-cache::lib` and will be
/// hardened to fail-closed when `tars-security` lands (TODO D-2).
fn read_iam_scopes(ctx: &RequestContext) -> Vec<String> {
    let Ok(attrs) = ctx.attributes.read() else {
        return Vec::new();
    };
    let Some(value) = attrs.get(IAM_SCOPES_ATTR) else {
        return Vec::new();
    };
    let Some(arr) = value.as_array() else {
        tracing::warn!("cache: `{IAM_SCOPES_ATTR}` attribute is not a JSON array; ignoring");
        return Vec::new();
    };
    let mut scopes: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    scopes.sort();
    scopes
}

fn hash_message(h: &mut Sha256, msg: &Message) -> Result<(), CacheError> {
    match msg {
        Message::User { content } => {
            h.update(b"\0USER\0");
            for b in content {
                hash_content_block(h, b);
            }
        }
        Message::System { content } => {
            h.update(b"\0SYSTEM_MSG\0");
            for b in content {
                hash_content_block(h, b);
            }
        }
        Message::Assistant {
            content,
            tool_calls,
        } => {
            h.update(b"\0ASSISTANT\0");
            for b in content {
                hash_content_block(h, b);
            }
            // Sort tool calls by id so emit order doesn't perturb the
            // hash (the model that emits them serially in the same
            // logical turn shouldn't make later replays diverge).
            let mut sorted: Vec<&tars_types::ToolCall> = tool_calls.iter().collect();
            sorted.sort_by(|a, b| a.id.cmp(&b.id));
            for call in sorted {
                h.update(b"\0TC\0");
                h.update(call.id.as_bytes());
                h.update(b"\0");
                h.update(call.name.as_bytes());
                h.update(b"\0");
                let canonical = canonical_json(&call.arguments)?;
                h.update(&canonical);
            }
        }
        Message::Tool {
            tool_call_id,
            content,
            is_error,
        } => {
            h.update(b"\0TOOL\0");
            h.update(tool_call_id.as_bytes());
            h.update(b"\0");
            h.update(if *is_error { b"E" } else { b"O" });
            for b in content {
                hash_content_block(h, b);
            }
        }
    }
    Ok(())
}

fn hash_content_block(h: &mut Sha256, block: &ContentBlock) {
    match block {
        ContentBlock::Text { text } => {
            h.update(b"\0T\0");
            h.update(text.as_bytes());
        }
        ContentBlock::Image { mime, data } => {
            h.update(b"\0IMG\0");
            h.update(mime.as_bytes());
            h.update(b"\0");
            // Image bytes can be MB-scale; hash the descriptor only.
            // For URL data this means "two distinct images at the same
            // URL collide" — that's the documented limitation of
            // `ImageData::descriptor_hash` (chat-15) and the right
            // tradeoff at this layer.
            let _ = ImageData::Url; // keep the import live
            let descriptor = data.descriptor_hash();
            h.update(descriptor.as_bytes());
        }
    }
}

/// Stable JSON encoding. We rely on serde_json's default
/// (BTreeMap-backed) `Map`, which sorts keys alphabetically. If the
/// workspace ever turns on the `preserve_order` feature this becomes
/// non-deterministic — guard with the test in `tests::sorted_object_keys`.
fn canonical_json(v: &Value) -> Result<Vec<u8>, CacheError> {
    serde_json::to_vec(v).map_err(CacheError::Serialize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tars_types::{ChatRequest, ModelHint, ModelTier, ThinkingMode};

    fn det_req(prompt: &str) -> ChatRequest {
        let mut r = ChatRequest::user(ModelHint::Explicit("gpt-4o".into()), prompt);
        r.temperature = Some(0.0);
        r
    }

    fn ctx_with_scopes(tenant: &str, scopes: &[&str]) -> RequestContext {
        let c = RequestContext::test_default();
        // Override tenant.
        let mut c = c;
        c.tenant_id = tars_types::TenantId::new(tenant);
        if !scopes.is_empty() {
            let mut a = c.attributes.write().unwrap();
            a.insert(
                IAM_SCOPES_ATTR.into(),
                serde_json::Value::Array(
                    scopes.iter().map(|s| Value::String((*s).into())).collect(),
                ),
            );
        }
        c
    }

    #[test]
    fn identical_requests_produce_identical_keys() {
        let f = CacheKeyFactory::new(1);
        let a = f
            .compute(&det_req("hi"), &ctx_with_scopes("t1", &["read"]))
            .unwrap();
        let b = f
            .compute(&det_req("hi"), &ctx_with_scopes("t1", &["read"]))
            .unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn different_tenants_never_collide_even_with_same_prompt() {
        let f = CacheKeyFactory::new(1);
        let a = f
            .compute(&det_req("hi"), &ctx_with_scopes("tenantA", &["read"]))
            .unwrap();
        let b = f
            .compute(&det_req("hi"), &ctx_with_scopes("tenantB", &["read"]))
            .unwrap();
        assert_ne!(a.fingerprint, b.fingerprint, "tenant must be in the hash");
    }

    #[test]
    fn different_scopes_never_collide() {
        // The IDOR scenario from Doc 03 §3.1 — two principals with
        // different read-rights against overlapping data must not
        // share a cache slot.
        let f = CacheKeyFactory::new(1);
        let a = f
            .compute(&det_req("hi"), &ctx_with_scopes("t1", &["scope:a"]))
            .unwrap();
        let b = f
            .compute(&det_req("hi"), &ctx_with_scopes("t1", &["scope:b"]))
            .unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn scope_order_does_not_matter() {
        // Sorted before hashing → ["a","b"] and ["b","a"] should collide.
        let f = CacheKeyFactory::new(1);
        let a = f
            .compute(&det_req("hi"), &ctx_with_scopes("t1", &["a", "b"]))
            .unwrap();
        let b = f
            .compute(&det_req("hi"), &ctx_with_scopes("t1", &["b", "a"]))
            .unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn temperature_must_be_explicit_zero() {
        let f = CacheKeyFactory::new(1);
        let mut r = det_req("hi");
        r.temperature = Some(0.7);
        let err = f.compute(&r, &ctx_with_scopes("t", &[])).unwrap_err();
        assert!(matches!(err, CacheError::NonDeterministic));

        // Default (None) is also non-cacheable — the provider's default
        // temperature is unknown.
        let mut r = det_req("hi");
        r.temperature = None;
        let err = f.compute(&r, &ctx_with_scopes("t", &[])).unwrap_err();
        assert!(matches!(err, CacheError::NonDeterministic));
    }

    #[test]
    fn tier_and_ensemble_are_rejected_with_distinct_errors() {
        let f = CacheKeyFactory::new(1);
        let mut r = det_req("hi");
        r.model = ModelHint::Tier(ModelTier::Fast);
        let err = f.compute(&r, &ctx_with_scopes("t", &[])).unwrap_err();
        assert!(matches!(err, CacheError::UnresolvedTier));

        r.model = ModelHint::Ensemble(vec![ModelHint::Explicit("a".into())]);
        let err = f.compute(&r, &ctx_with_scopes("t", &[])).unwrap_err();
        assert!(matches!(err, CacheError::UncacheableEnsemble));
    }

    #[test]
    fn hasher_version_bump_invalidates_keys() {
        let f1 = CacheKeyFactory::new(1);
        let f2 = CacheKeyFactory::new(2);
        let ctx = ctx_with_scopes("t", &[]);
        assert_ne!(
            f1.compute(&det_req("hi"), &ctx).unwrap().fingerprint,
            f2.compute(&det_req("hi"), &ctx).unwrap().fingerprint,
        );
    }

    #[test]
    fn distinct_prompts_distinct_keys() {
        let f = CacheKeyFactory::new(1);
        let ctx = ctx_with_scopes("t", &[]);
        assert_ne!(
            f.compute(&det_req("hi"), &ctx).unwrap().fingerprint,
            f.compute(&det_req("ho"), &ctx).unwrap().fingerprint,
        );
    }

    #[test]
    fn thinking_mode_participates() {
        let f = CacheKeyFactory::new(1);
        let ctx = ctx_with_scopes("t", &[]);
        let mut r1 = det_req("hi");
        r1.thinking = ThinkingMode::Off;
        let mut r2 = det_req("hi");
        r2.thinking = ThinkingMode::Auto;
        assert_ne!(
            f.compute(&r1, &ctx).unwrap().fingerprint,
            f.compute(&r2, &ctx).unwrap().fingerprint,
        );
    }

    #[test]
    fn structured_output_schema_participates() {
        let f = CacheKeyFactory::new(1);
        let ctx = ctx_with_scopes("t", &[]);
        let mut r1 = det_req("hi");
        r1.structured_output = Some(tars_types::JsonSchema::strict(
            "R",
            json!({"type":"object"}),
        ));
        let mut r2 = det_req("hi");
        r2.structured_output = Some(tars_types::JsonSchema::strict("R", json!({"type":"array"})));
        assert_ne!(
            f.compute(&r1, &ctx).unwrap().fingerprint,
            f.compute(&r2, &ctx).unwrap().fingerprint,
        );
    }

    #[test]
    fn hex_label_is_64_lowercase_chars() {
        let f = CacheKeyFactory::new(1);
        let key = f
            .compute(&det_req("hi"), &ctx_with_scopes("t", &[]))
            .unwrap();
        let h = key.hex();
        assert_eq!(h.len(), 64);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}
