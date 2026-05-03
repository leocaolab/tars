//! Secret references and protected strings.
//!
//! [`SecretRef`] describes *where* a secret can be fetched from; it
//! never contains the value itself in the long-lived case (env / file
//! variants). The one exception, [`SecretRef::Inline`], holds the
//! plaintext directly — useful for tests and dev configs, but the
//! contained [`SecretString`] redacts on Display/Debug so a stray
//! `tracing::info!(secret = %s, …)` doesn't leak it.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Pointer to where a secret value lives. Resolved (potentially
/// asynchronously) at use time by an external resolver — Provider
/// layer never touches a `SecretRef` directly except to forward it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretRef {
    /// Plaintext embedded in config. Test/dev only — production should
    /// emit a startup warning if any inline secret is observed.
    Inline { value: SecretString },
    /// `$VAR` resolved from process env at first use.
    Env { var: String },
    /// File on disk; entire contents are the secret (trimmed of trailing
    /// newline). Useful for K8s secret mounts.
    File { path: PathBuf },
}

impl SecretRef {
    /// Sugar for the env-var variant.
    pub fn env(var: impl Into<String>) -> Self {
        Self::Env { var: var.into() }
    }

    /// Sugar for the inline variant; keep usage to tests.
    pub fn inline(value: impl Into<String>) -> Self {
        Self::Inline { value: SecretString::new(value.into()) }
    }
}

/// String wrapper whose `Display` / `Debug` outputs redact the value.
/// Use this for any plaintext credential that shouldn't end up in
/// logs / error chains / panic messages.
///
/// **Caveat**: this is a runtime hygiene layer, not a memory-protection
/// guarantee. A determined attacker with process memory access can
/// still read the bytes. The goal is to prevent *accidental* leakage
/// via the standard formatting machinery.
/// `Serialize` is implemented manually to emit `"<redacted>"` rather
/// than the plaintext — the audit (`tars-types-src-secret-2`) called
/// out the original `#[serde(transparent)]` derive as the primary
/// leak path. `Deserialize` stays transparent so config files
/// (`{value = "sk-…"}`) continue to load. We never round-trip a secret
/// in this codebase: configs flow read-only from disk through
/// runtime, and any request/response/cache JSON we emit must not
/// contain a secret.
#[derive(Clone)]
pub struct SecretString(String);

impl Serialize for SecretString {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("<redacted>")
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(String::deserialize(d)?))
    }
}

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the raw value. Be deliberate: anything you do with this
    /// reference can leak the secret if mishandled.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Move the raw value out. Same warning as [`Self::expose`].
    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<secret:{}>", self.0.len())
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretString(<redacted:{}>)", self.0.len())
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_debug_redact() {
        let s = SecretString::new("super-secret-key");
        assert_eq!(format!("{s}"), "<secret:16>");
        assert!(format!("{s:?}").contains("redacted"));
        assert!(!format!("{s:?}").contains("super-secret-key"));
        assert!(!format!("{s}").contains("super-secret-key"));
    }

    #[test]
    fn ref_round_trips_through_serde() {
        let r = SecretRef::env("OPENAI_API_KEY");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["source"], "env");
        assert_eq!(v["var"], "OPENAI_API_KEY");
        let back: SecretRef = serde_json::from_value(v).unwrap();
        match back {
            SecretRef::Env { var } => assert_eq!(var, "OPENAI_API_KEY"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn inline_serializes_redacted_not_plaintext() {
        // Audit `tars-types-src-secret-2`: previous behaviour
        // serialized the plaintext via #[serde(transparent)]. The
        // contract now is "Serialize redacts; Deserialize accepts".
        let r = SecretRef::inline("sk-proj-x");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["value"], "<redacted>");
        assert!(
            !serde_json::to_string(&r).unwrap().contains("sk-proj-x"),
            "plaintext must not appear anywhere in the JSON",
        );
    }

    #[test]
    fn inline_deserialize_still_accepts_plaintext() {
        // Configs on disk look like `{value = "sk-…"}` — the load path
        // must keep working after the Serialize redaction landed.
        let r: SecretRef = serde_json::from_value(serde_json::json!({
            "source": "inline",
            "value": "sk-proj-x"
        }))
        .unwrap();
        match r {
            SecretRef::Inline { value } => assert_eq!(value.expose(), "sk-proj-x"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn ref_deserializes_from_toml_struct_form() {
        let toml_str = r#"
            source = "file"
            path = "/run/secrets/key"
        "#;
        let r: SecretRef = toml::from_str(toml_str).unwrap();
        match r {
            SecretRef::File { path } => assert_eq!(path.to_str().unwrap(), "/run/secrets/key"),
            _ => panic!("wrong variant"),
        }
    }
}
