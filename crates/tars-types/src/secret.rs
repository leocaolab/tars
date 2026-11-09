//! Names where a secret comes from ([`SecretRef`]) and holds a plaintext one ([`SecretString`])
//! so that formatting or serializing it never prints the value. This module does not read env
//! vars or files; the resolver in tars-llm does.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Where a secret value lives. An auth resolver reads it at use time; everything else only
/// forwards it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretRef {
    /// The plaintext itself, for tests and dev configs; the resolver warns once when it reads one.
    Inline {
        value: SecretString,
    },
    Env {
        var: String,
    },
    /// A file whose whole contents, trimmed of surrounding whitespace, are the secret, as in a
    /// K8s secret mount.
    File {
        path: PathBuf,
    },
}

impl SecretRef {
    pub fn env(var: impl Into<String>) -> Self {
        Self::Env { var: var.into() }
    }

    pub fn inline(value: impl Into<String>) -> Self {
        Self::Inline {
            value: SecretString::new(value.into()),
        }
    }
}

/// A plaintext credential whose `Display`, `Debug` and `Serialize` never print the value, so it
/// stays out of logs, error chains, panic messages and emitted JSON.
///
/// `Serialize` writes `"<redacted>"` while `Deserialize` reads the plaintext, so a config file
/// (`{value = "sk-…"}`) loads but a serialized secret does not round-trip. This guards against
/// accidental leaks through formatting, not against a reader of process memory.
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

    /// Returns the plaintext; whatever the caller does with it can leak the secret.
    pub fn expose(&self) -> &str {
        &self.0
    }

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
