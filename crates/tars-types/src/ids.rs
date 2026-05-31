//! Strongly typed identifiers.
//!
//! Every "ID" in the system is its own type — never raw strings — so the
//! type-checker prevents passing a `TenantId` where a `SessionId` is
//! expected. This is the cheapest correctness measure available and pays
//! for itself the first time someone refactors a function signature.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Generate a newtype wrapping a `String`, with `Display` / `Debug` /
/// `serde` / `From<&str>` / `As<&str>` boilerplate.
macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        // `Serialize` stays transparent (emit the bare string). For
        // `Deserialize` we hand-roll via `TryFrom<String>` so the same
        // empty-string rejection that `new()` enforces also fires on
        // the wire — a plain `#[serde(transparent)]` Deserialize would
        // let `from_str("\"\"")` mint an empty ID, bypassing the hard
        // isolation boundary (audit `tars-types-src-ids-1`).
        //
        // We don't combine `transparent` + `try_from` on one derive:
        // serde rejects that pairing. Serialize keeps transparent;
        // Deserialize is a separate manual impl that reuses the inner
        // `String` deserializer then validates.
        #[derive(Clone, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let s = String::deserialize(deserializer)?;
                if s.is_empty() {
                    return Err(serde::de::Error::custom(concat!(
                        stringify!($name),
                        " cannot be empty"
                    )));
                }
                Ok(Self(s))
            }
        }

        impl $name {
            /// Construct from any string-like value.
            ///
            /// Panics on the empty string. ID semantics require a
            /// non-empty value (see audit `tars-types-src-ids-1`); an
            /// empty `TenantId` / `SessionId` etc. would propagate
            /// silently into cache keys, IAM scope checks, and DB
            /// lookups where it'd manifest as obscure correctness bugs.
            /// Failing fast at construction is cheaper than chasing
            /// the symptom three layers down.
            pub fn new(value: impl Into<String>) -> Self {
                let v: String = value.into();
                assert!(!v.is_empty(), "{} cannot be empty", stringify!($name),);
                Self(v)
            }

            /// Borrow the underlying string.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Move the underlying string out.
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    TenantId,
    "Tenant identifier — the hard isolation boundary (Doc 06 §3)."
);
string_id!(SessionId, "Session identifier (Doc 06 §3.3).");
string_id!(
    TraceId,
    "Distributed trace identifier; propagated through all layers."
);
string_id!(TaskId, "Task identifier (Doc 04 submit handle).");
string_id!(TrajectoryId, "Trajectory node identifier (Doc 04 §3.1).");
string_id!(PrincipalId, "Principal (caller identity) identifier.");
string_id!(
    ProviderId,
    "Provider instance identifier (e.g. `openai_main`, `local_qwen`)."
);
string_id!(
    L3HandleId,
    "Internal handle for an L3 explicit cache (Doc 03 §7)."
);
string_id!(
    AgentId,
    "Agent instance identifier (Doc 04 §4 — `orchestrator`, `worker:code_review`, etc.)."
);
string_id!(
    BatchJobId,
    "Batch job identifier returned by the provider on submit (Doc 01 §6.3 / roadmap §5)."
);
string_id!(
    BatchItemId,
    "Per-item identifier inside a batch — caller-chosen, echoed back in results so each output can be matched to its input. Maps to Anthropic's `custom_id` and OpenAI's `custom_id` on batch lines."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_distinct_types() {
        let t = TenantId::new("acme");
        let s = SessionId::new("acme");
        // The fact that this compiles is the test:
        //   `assert_eq!(t, s)` would fail to compile.
        assert_eq!(t.as_str(), s.as_str());
    }

    #[test]
    fn ids_round_trip_through_serde() {
        let t = TenantId::new("acme");
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"acme\"");
        let back: TenantId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn debug_includes_type_name() {
        let t = TenantId::new("acme");
        assert_eq!(format!("{:?}", t), "TenantId(\"acme\")");
    }

    #[test]
    #[should_panic(expected = "TenantId cannot be empty")]
    fn empty_id_panics_at_construction() {
        let _ = TenantId::new("");
    }

    #[test]
    fn empty_id_via_serde_is_rejected_not_minted() {
        // `#[serde(transparent)]` Deserialize would have happily minted
        // an empty TenantId here, bypassing the hard isolation boundary.
        let r: Result<TenantId, _> = serde_json::from_str("\"\"");
        assert!(r.is_err(), "empty string must not deserialize into an ID");
    }

    #[test]
    #[should_panic(expected = "SessionId cannot be empty")]
    fn empty_id_via_from_also_panics() {
        // From<&str> routes through new() so the same guard fires.
        let _: SessionId = SessionId::from("");
    }
}
