//! Says how a provider authenticates, without resolving anything: tars-llm resolves an
//! [`Auth`] into its `ResolvedAuth` and the adapter writes the headers.

use serde::{Deserialize, Serialize};

use crate::secret::SecretRef;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    /// The backend does not authenticate, as with a local OpenAI-compatible server or the mock.
    None,
    /// The underlying tool or CLI uses the credential it already has (a `claude login`
    /// session); tars never sees it.
    Delegate,
    /// An API key or bearer token read from a [`SecretRef`].
    Secret { secret: SecretRef },
}

impl Auth {
    /// Returns a secret read from the env var `var`.
    pub fn env(var: impl Into<String>) -> Self {
        Self::Secret {
            secret: SecretRef::env(var),
        }
    }

    /// Returns an inline plaintext secret, for tests and dev configs.
    pub fn inline(value: impl Into<String>) -> Self {
        Self::Secret {
            secret: SecretRef::inline(value),
        }
    }

    /// Returns a secret read from the file at `path`.
    pub fn file(path: impl Into<std::path::PathBuf>) -> Self {
        Self::Secret {
            secret: SecretRef::File { path: path.into() },
        }
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
