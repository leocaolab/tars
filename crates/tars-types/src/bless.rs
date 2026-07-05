//! The **bless store** — loadable, committed field-level assertions about a
//! *pinned* response. Doc 28.
//!
//! A [`Bless`] is a JSON file of `{selector, expected, match}` assertions. A
//! test decodes a (cassette-replayed → deterministic) response into a
//! [`serde_json::Value`] and calls [`Bless::check`]; an empty [`BlessOutcome`]
//! is a pass, any [`Drift`] is a fail naming `(selector, expected, actual)`.
//!
//! Layers (each generic, no domain types):
//! - [`Codec`] tags the wire format; tars ships JSON only. Selectors are a
//!   JSONPath **subset** (`$.a.b`, `$['a']`, `[N]`) resolved internally;
//!   a full RFC-9535 engine can replace it without touching [`Bless`], and a
//!   proto field-path impl is possible for a consumer but out of scope (Doc 28 §14).
//! - [`MatchTier`] is how-equal: `exact` / `normalized` / `semantic`.
//! - [`Bless`] is the file + `check`.
//!
//! Fail-closed (Doc 28 NFR-2): a selector that resolves to nothing is a
//! [`Drift`], never a silent pass; a `semantic` assert in the pure
//! [`check`](Bless::check) is reported unresolved (fail-closed) until the
//! judge-aware path (M2) evaluates it.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Typed failure of the bless family.
#[derive(Debug, thiserror::Error)]
pub enum BlessError {
    /// The bless file could not be read/written.
    #[error("bless file io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The bless file was not valid JSON / did not match the schema.
    #[error("bless file is not valid: {source}")]
    Parse {
        #[source]
        source: serde_json::Error,
    },
    /// A selector used syntax the shipped [`JsonPathCodec`] subset does not
    /// support. Loud on purpose — a typo must not silently match nothing.
    #[error("unsupported selector {selector:?}: {reason}")]
    BadSelector { selector: String, reason: String },
}

/// Wire codec a bless is expressed over. tars ships [`Json`](Codec::Json) only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    /// JSON value + JSONPath-subset selectors.
    #[default]
    Json,
}

/// How strictly an [`Assert`]'s `expected` must match the resolved value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchTier {
    /// `serde_json::Value` equality.
    #[default]
    Exact,
    /// Equality after canonicalizing whitespace + number representation
    /// (so `8` == `8.0`, `"a  b"` == `"a b"`).
    Normalized,
    /// LLM-judged equivalence (free-form text). Evaluated only by the
    /// judge-aware path; the pure [`check`](Bless::check) reports it unresolved.
    Semantic,
}

/// One field-level assertion: the `expected` value at `selector`, compared at
/// `tier`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Assert {
    /// A JSONPath-subset selector, e.g. `"$.severity"` or `"$.items[0].id"`.
    pub selector: String,
    /// The blessed value.
    pub expected: Value,
    /// Match strictness; defaults to [`MatchTier::Exact`].
    #[serde(rename = "match", default)]
    pub tier: MatchTier,
}

/// A committed bless file: field-level assertions about one pinned response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bless {
    /// Wire codec (JSON). Defaults to [`Codec::Json`].
    #[serde(default)]
    pub codec: Codec,
    /// The cassette [`request_fingerprint`] this was blessed from — provenance,
    /// so a stale bless (fingerprint no longer recorded) can be flagged.
    ///
    /// [`request_fingerprint`]: (see tars-provider cassette)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<String>,
    /// The assertions; all must hold for a pass.
    pub asserts: Vec<Assert>,
}

/// One failed assertion.
#[derive(Clone, Debug, PartialEq)]
pub struct Drift {
    /// The selector that drifted.
    pub selector: String,
    /// What the bless expected.
    pub expected: Value,
    /// What the value actually had (`None` = selector resolved to nothing).
    pub actual: Option<Value>,
    /// The tier at which the comparison was made.
    pub tier: MatchTier,
    /// Human-readable why (e.g. "missing", "not equal", "semantic: needs judge").
    pub reason: String,
}

/// Result of [`Bless::check`] — empty `drifts` is a pass.
#[derive(Clone, Debug, Default)]
pub struct BlessOutcome {
    /// Every assertion that did not hold.
    pub drifts: Vec<Drift>,
}

impl BlessOutcome {
    /// `true` when nothing drifted.
    pub fn is_pass(&self) -> bool {
        self.drifts.is_empty()
    }
}

impl Bless {
    /// Load a bless from a committed JSON file.
    pub fn load(path: &Path) -> Result<Self, BlessError> {
        let raw = std::fs::read_to_string(path).map_err(|source| BlessError::Io {
            path: path.display().to_string(),
            source,
        })?;
        serde_json::from_str(&raw).map_err(|source| BlessError::Parse { source })
    }

    /// Serialize to a stable, diff-friendly JSON string (sorted keys via
    /// serde_json's pretty printer over a `Bless` with a fixed field order).
    /// Caller decides the path (record writes `*.bless.new`, never in place).
    pub fn to_json(&self) -> Result<String, BlessError> {
        serde_json::to_string_pretty(self).map_err(|source| BlessError::Parse { source })
    }

    /// Build a bless by capturing the current value at each `selector`
    /// (all [`MatchTier::Exact`]). Refuses a selector that resolves to nothing
    /// — Doc 28 §8: don't freeze an absent field.
    pub fn capture(
        value: &Value,
        selectors: &[&str],
        source_fingerprint: Option<String>,
    ) -> Result<Self, BlessError> {
        let mut asserts = Vec::with_capacity(selectors.len());
        for sel in selectors {
            match resolve(value, sel)? {
                Some(v) => asserts.push(Assert {
                    selector: (*sel).to_string(),
                    expected: v.clone(),
                    tier: MatchTier::Exact,
                }),
                None => {
                    return Err(BlessError::BadSelector {
                        selector: (*sel).to_string(),
                        reason: "resolves to nothing — refusing to bless an absent field".into(),
                    });
                }
            }
        }
        Ok(Self { codec: Codec::Json, source_fingerprint, asserts })
    }

    /// Check `value` against every assertion — exact/normalized only; a
    /// [`Semantic`](MatchTier::Semantic) assert is a fail-closed unresolved
    /// drift. Empty [`BlessOutcome`] ⇒ pass. See [`check_with`](Bless::check_with)
    /// to supply a semantic judge.
    pub fn check(&self, value: &Value) -> Result<BlessOutcome, BlessError> {
        self.check_with(value, |_expected, _actual| false)
    }

    /// Like [`check`](Bless::check), but `judge(expected, actual) -> bool`
    /// resolves [`Semantic`](MatchTier::Semantic) asserts. The judge lives
    /// *above* `tars-types` (it's an LLM call), so it's injected as a closure —
    /// the eval layer passes one backed by a real judge provider (Doc 28 §6 C3).
    pub fn check_with(
        &self,
        value: &Value,
        judge: impl Fn(&Value, &Value) -> bool,
    ) -> Result<BlessOutcome, BlessError> {
        let mut drifts = Vec::new();
        for a in &self.asserts {
            let Some(actual) = resolve(value, &a.selector)? else {
                drifts.push(Drift {
                    selector: a.selector.clone(),
                    expected: a.expected.clone(),
                    actual: None,
                    tier: a.tier,
                    reason: "missing".into(),
                });
                continue;
            };
            let ok = match a.tier {
                MatchTier::Exact => actual == &a.expected,
                MatchTier::Normalized => canon(actual) == canon(&a.expected),
                MatchTier::Semantic => judge(&a.expected, actual),
            };
            if !ok {
                drifts.push(Drift {
                    selector: a.selector.clone(),
                    expected: a.expected.clone(),
                    actual: Some(actual.clone()),
                    tier: a.tier,
                    reason: match a.tier {
                        MatchTier::Semantic => "semantically not equivalent".into(),
                        _ => "not equal".into(),
                    },
                });
            }
        }
        Ok(BlessOutcome { drifts })
    }
}

/// The pending-file suffix (insta's `.snap.new` pattern): a capture is written
/// here first and only an explicit accept promotes it, so a crashed record can
/// never clobber a committed bless (Doc 28 §12).
pub const PENDING_SUFFIX: &str = ".new";

/// `<path>.new` — where a capture is staged before acceptance.
pub fn pending_path(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(PENDING_SUFFIX);
    std::path::PathBuf::from(s)
}

impl Bless {
    /// Write the accepted bless to `path` (pretty, stable formatting).
    pub fn save(&self, path: &Path) -> Result<(), BlessError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| BlessError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        std::fs::write(path, self.to_json()?).map_err(|source| BlessError::Io {
            path: path.display().to_string(),
            source,
        })
    }

    /// Stage a capture to `<path>.new` (never touches the committed `path`).
    /// Returns the pending path for the caller to review + [`accept_pending`].
    pub fn save_pending(&self, path: &Path) -> Result<std::path::PathBuf, BlessError> {
        let pending = pending_path(path);
        self.save(&pending)?;
        Ok(pending)
    }

    /// The test/CLI-facing "approval assert" (CUJ-2/4). Mirrors insta:
    /// - `do_bless` (e.g. `TARS_BLESS=1`): capture `selectors` from `value`,
    ///   promote to `path`, return a pass. The git diff of `path` is the review.
    /// - else if `path` exists: load + [`check`](Bless::check) it.
    /// - else: error (Doc 28 FR-6 — CI must not silently create a bless).
    pub fn check_or_bless(
        path: &Path,
        value: &Value,
        selectors: &[&str],
        source_fingerprint: Option<String>,
        do_bless: bool,
    ) -> Result<BlessOutcome, BlessError> {
        if do_bless {
            let staged = Self::capture(value, selectors, source_fingerprint)?;
            let pending = staged.save_pending(path)?;
            accept_pending(&pending, path)?;
            return Ok(BlessOutcome::default());
        }
        if path.exists() {
            return Self::load(path)?.check(value);
        }
        Err(BlessError::Io {
            path: path.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no bless file — re-run with bless enabled (TARS_BLESS=1) to create it",
            ),
        })
    }
}

/// Promote a staged `<path>.new` to the committed `path` (atomic rename).
pub fn accept_pending(pending: &Path, path: &Path) -> Result<(), BlessError> {
    std::fs::rename(pending, path).map_err(|source| BlessError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Canonicalize a value for [`MatchTier::Normalized`]: numbers to `f64` shape,
/// strings whitespace-collapsed, recursing into arrays/objects.
fn canon(v: &Value) -> Value {
    match v {
        Value::Number(n) => n
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| v.clone()),
        Value::String(s) => Value::String(s.split_whitespace().collect::<Vec<_>>().join(" ")),
        Value::Array(a) => Value::Array(a.iter().map(canon).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), canon(x))).collect()),
        _ => v.clone(),
    }
}

/// Resolve a JSONPath-**subset** selector against `value`. Supported:
/// `$` (root), `.name`, `['name']` / `["name"]`, `[N]`. Anything else is a
/// [`BlessError::BadSelector`] — a typo fails loud, never matches nothing.
///
/// This is the [`JsonPathCodec`] impl; the [`Codec`] seam lets a full engine
/// (e.g. `serde_json_path`) replace it without touching [`Bless`].
fn resolve<'v>(value: &'v Value, selector: &str) -> Result<Option<&'v Value>, BlessError> {
    let segs = parse_segments(selector)?;
    let mut cur = value;
    for seg in segs {
        let next = match seg {
            Seg::Key(k) => cur.get(&k),
            Seg::Index(i) => cur.get(i),
        };
        match next {
            Some(v) => cur = v,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

enum Seg {
    Key(String),
    Index(usize),
}

fn parse_segments(selector: &str) -> Result<Vec<Seg>, BlessError> {
    let bad = |reason: &str| BlessError::BadSelector {
        selector: selector.to_string(),
        reason: reason.to_string(),
    };
    let s = selector.trim();
    let mut rest = s.strip_prefix('$').ok_or_else(|| bad("must start with '$'"))?;
    let mut segs = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            // .name  (name = alnum / '_' / '-'), stop at next '.' or '['
            let end = after.find(['.', '[']).unwrap_or(after.len());
            let name = &after[..end];
            if name.is_empty() {
                return Err(bad("empty '.name' segment"));
            }
            segs.push(Seg::Key(name.to_string()));
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let close = after.find(']').ok_or_else(|| bad("unclosed '['"))?;
            let inner = after[..close].trim();
            let seg = if (inner.starts_with('\'') && inner.ends_with('\''))
                || (inner.starts_with('"') && inner.ends_with('"'))
            {
                Seg::Key(inner[1..inner.len() - 1].to_string())
            } else {
                let i: usize = inner
                    .parse()
                    .map_err(|_| bad("bracket must be a quoted key or a numeric index"))?;
                Seg::Index(i)
            };
            segs.push(seg);
            rest = &after[close + 1..];
        } else {
            return Err(bad("expected '.' or '[' between segments"));
        }
    }
    Ok(segs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bless_json(sev: i64) -> Bless {
        Bless {
            codec: Codec::Json,
            source_fingerprint: Some("abc123".into()),
            asserts: vec![Assert {
                selector: "$.severity".into(),
                expected: json!(sev),
                tier: MatchTier::Exact,
            }],
        }
    }

    // ── resolver ────────────────────────────────────────────────────
    #[test]
    fn resolves_dotted_bracketed_and_index() {
        let v = json!({"a": {"b": 1}, "arr": [10, 20], "k-1": {"x": 9}});
        assert_eq!(resolve(&v, "$.a.b").unwrap(), Some(&json!(1)));
        assert_eq!(resolve(&v, "$.arr[1]").unwrap(), Some(&json!(20)));
        assert_eq!(resolve(&v, "$['k-1'].x").unwrap(), Some(&json!(9)));
        assert_eq!(resolve(&v, "$").unwrap(), Some(&v));
    }

    #[test]
    fn missing_path_resolves_to_none() {
        let v = json!({"a": 1});
        assert_eq!(resolve(&v, "$.b").unwrap(), None);
        assert_eq!(resolve(&v, "$.a.b").unwrap(), None);
    }

    #[test]
    fn bad_selector_is_loud() {
        let v = json!({"a": 1});
        assert!(matches!(resolve(&v, "a").unwrap_err(), BlessError::BadSelector { .. }));
        assert!(matches!(resolve(&v, "$.a[bad]").unwrap_err(), BlessError::BadSelector { .. }));
    }

    // ── check: exact (CUJ-2/3) ──────────────────────────────────────
    #[test]
    fn exact_match_passes() {
        let out = bless_json(8).check(&json!({"severity": 8, "summary": "x"})).unwrap();
        assert!(out.is_pass(), "{:?}", out.drifts);
    }

    #[test]
    fn exact_drift_reports_expected_and_actual() {
        let out = bless_json(8).check(&json!({"severity": 9})).unwrap();
        assert_eq!(out.drifts.len(), 1);
        let d = &out.drifts[0];
        assert_eq!(d.selector, "$.severity");
        assert_eq!(d.expected, json!(8));
        assert_eq!(d.actual, Some(json!(9)));
    }

    #[test]
    fn missing_field_is_a_drift_not_a_pass() {
        // Fail-closed (FR-3).
        let out = bless_json(8).check(&json!({"summary": "no severity here"})).unwrap();
        assert_eq!(out.drifts.len(), 1);
        assert_eq!(out.drifts[0].actual, None);
        assert_eq!(out.drifts[0].reason, "missing");
    }

    // ── check: normalized ───────────────────────────────────────────
    #[test]
    fn normalized_matches_int_vs_float_and_whitespace() {
        let b = Bless {
            codec: Codec::Json,
            source_fingerprint: None,
            asserts: vec![
                Assert { selector: "$.n".into(), expected: json!(8), tier: MatchTier::Normalized },
                Assert {
                    selector: "$.s".into(),
                    expected: json!("a b"),
                    tier: MatchTier::Normalized,
                },
            ],
        };
        let out = b.check(&json!({"n": 8.0, "s": "a   b"})).unwrap();
        assert!(out.is_pass(), "{:?}", out.drifts);
    }

    // ── check: semantic is fail-closed in the pure path ─────────────
    #[test]
    fn semantic_is_unresolved_drift_without_judge() {
        let b = Bless {
            codec: Codec::Json,
            source_fingerprint: None,
            asserts: vec![Assert {
                selector: "$.summary".into(),
                expected: json!("worker crash"),
                tier: MatchTier::Semantic,
            }],
        };
        // pure check() is fail-closed for semantic
        let out = b.check(&json!({"summary": "the worker panics"})).unwrap();
        assert_eq!(out.drifts.len(), 1);
        // but check_with a judge resolves it
        let out = b
            .check_with(&json!({"summary": "the worker panics"}), |_exp, _act| true)
            .unwrap();
        assert!(out.is_pass(), "judge accepted → pass");
    }

    // ── capture (CUJ-1) ─────────────────────────────────────────────
    #[test]
    fn capture_freezes_selected_fields() {
        let v = json!({"severity": 8, "summary": "x", "extra": 1});
        let b = Bless::capture(&v, &["$.severity", "$.summary"], Some("fp".into())).unwrap();
        assert_eq!(b.asserts.len(), 2);
        assert_eq!(b.asserts[0].expected, json!(8));
        // and it round-trips through the file format
        let round: Bless = serde_json::from_str(&b.to_json().unwrap()).unwrap();
        assert!(round.check(&v).unwrap().is_pass());
    }

    #[test]
    fn capture_refuses_absent_field() {
        let v = json!({"severity": 8});
        assert!(matches!(
            Bless::capture(&v, &["$.nope"], None).unwrap_err(),
            BlessError::BadSelector { .. }
        ));
    }

    // ── M1: record / re-bless via files (CUJ-1/4, E2E-3) ────────────
    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("tars_bless_{name}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.join("severity.bless.json")
    }

    #[test]
    fn check_or_bless_creates_then_passes_then_detects_drift() {
        let path = tmp("cycle");
        let v8 = json!({"severity": 8, "summary": "x"});

        // do_bless=true → writes the file, returns pass, and NO stray .new remains
        let out = Bless::check_or_bless(&path, &v8, &["$.severity"], None, true).unwrap();
        assert!(out.is_pass());
        assert!(path.exists(), "bless file created");
        assert!(!pending_path(&path).exists(), "pending promoted, not left behind");

        // do_bless=false + file exists → loads + checks; same value passes
        let out = Bless::check_or_bless(&path, &v8, &["$.severity"], None, false).unwrap();
        assert!(out.is_pass());

        // drift: severity moved → one drift 8 → 9
        let v9 = json!({"severity": 9});
        let out = Bless::check_or_bless(&path, &v9, &["$.severity"], None, false).unwrap();
        assert_eq!(out.drifts.len(), 1);
        assert_eq!(out.drifts[0].expected, json!(8));
        assert_eq!(out.drifts[0].actual, Some(json!(9)));

        // re-bless the intended change → file now expects 9 → passes
        Bless::check_or_bless(&path, &v9, &["$.severity"], None, true).unwrap();
        let out = Bless::check_or_bless(&path, &v9, &["$.severity"], None, false).unwrap();
        assert!(out.is_pass(), "re-blessed value passes");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn check_without_bless_and_no_file_is_an_error() {
        let path = tmp("missing");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap(); // ensure absent
        let err = Bless::check_or_bless(&path, &json!({"severity": 8}), &["$.severity"], None, false)
            .unwrap_err();
        assert!(matches!(err, BlessError::Io { .. }));
    }

    #[test]
    fn save_pending_never_touches_committed_file() {
        let path = tmp("pending");
        let b = bless_json(8);
        b.save(&path).unwrap();
        let committed = std::fs::read_to_string(&path).unwrap();
        // stage a different capture; committed file must be untouched until accept
        let staged = Bless::capture(&json!({"severity": 9}), &["$.severity"], None).unwrap();
        let pending = staged.save_pending(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), committed, "committed untouched");
        accept_pending(&pending, &path).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"expected\": 9"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn file_format_round_trips_with_match_rename() {
        let b = bless_json(8);
        let s = b.to_json().unwrap();
        assert!(s.contains("\"match\": \"exact\""), "match field renamed: {s}");
        let back: Bless = serde_json::from_str(&s).unwrap();
        assert_eq!(back.asserts[0].selector, "$.severity");
    }
}
