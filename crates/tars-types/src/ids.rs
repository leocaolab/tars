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
        #[derive(
            Clone,
            Eq,
            PartialEq,
            Hash,
            Ord,
            PartialOrd,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Construct from any string-like value.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
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
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(TenantId, "Tenant identifier — the hard isolation boundary (Doc 06 §3).");
string_id!(SessionId, "Session identifier (Doc 06 §3.3).");
string_id!(TraceId, "Distributed trace identifier; propagated through all layers.");
string_id!(TaskId, "Task identifier (Doc 04 submit handle).");
string_id!(TrajectoryId, "Trajectory node identifier (Doc 04 §3.1).");
string_id!(PrincipalId, "Principal (caller identity) identifier.");
string_id!(ProviderId, "Provider instance identifier (e.g. `openai_main`, `local_qwen`).");
string_id!(L3HandleId, "Internal handle for an L3 explicit cache (Doc 03 §7).");

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
}
