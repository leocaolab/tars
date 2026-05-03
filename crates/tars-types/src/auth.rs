//! Auth specifications.
//!
//! [`Auth`] is the *plan* for authentication: "use this secret as the
//! API key", "delegate to the underlying CLI", "no auth needed". The
//! concrete header injection happens at the Provider adapter — see
//! `tars-provider`'s `ResolvedAuth` for the runtime side.

use serde::{Deserialize, Serialize};

use crate::secret::SecretRef;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    /// Backend doesn't authenticate (local OpenAI-compatible servers,
    /// the in-process mock, etc.).
    None,
    /// Use whatever credentials the underlying tool / CLI already
    /// has (e.g. user's `claude login` session). Provider neither sees
    /// nor needs the credential.
    Delegate,
    /// Look up an API key / bearer token via [`SecretRef`].
    Secret { secret: SecretRef },
}

impl Auth {
    /// Sugar: use a process env var.
    pub fn env(var: impl Into<String>) -> Self {
        Self::Secret { secret: SecretRef::env(var) }
    }

    /// Sugar: inline plaintext (test/dev only).
    pub fn inline(value: impl Into<String>) -> Self {
        Self::Secret { secret: SecretRef::inline(value) }
    }

    /// Sugar: file-backed secret.
    pub fn file(path: impl Into<std::path::PathBuf>) -> Self {
        Self::Secret { secret: SecretRef::File { path: path.into() } }
    }

    /// True iff this Auth doesn't need to look anything up.
    pub fn is_passive(&self) -> bool {
        matches!(self, Self::None | Self::Delegate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_round_trips() {
        let a = Auth::None;
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["kind"], "none");
        let back: Auth = serde_json::from_value(v).unwrap();
        assert!(matches!(back, Auth::None));
    }

    #[test]
    fn secret_env_round_trips() {
        let a = Auth::env("OPENAI_API_KEY");
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["kind"], "secret");
        assert_eq!(v["secret"]["source"], "env");
        let back: Auth = serde_json::from_value(v).unwrap();
        match back {
            Auth::Secret {
                secret: SecretRef::Env { var },
            } => assert_eq!(var, "OPENAI_API_KEY"),
            _ => panic!("wrong"),
        }
    }

    #[test]
    fn passive_classifier() {
        assert!(Auth::None.is_passive());
        assert!(Auth::Delegate.is_passive());
        assert!(!Auth::env("X").is_passive());
    }

    #[test]
    fn deserializes_from_toml_struct_form() {
        let toml_str = r#"
            kind = "secret"
            secret = { source = "env", var = "ANTHROPIC_API_KEY" }
        "#;
        let a: Auth = toml::from_str(toml_str).unwrap();
        assert!(matches!(
            a,
            Auth::Secret { secret: SecretRef::Env { ref var } } if var == "ANTHROPIC_API_KEY"
        ));
    }
}
