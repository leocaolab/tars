//! Provider declarations. The schema is **mirrored** from the concrete
//! backends in `tars-provider` — adding a new provider type means
//! adding a variant here AND a builder in `tars-provider`. We keep
//! them in lockstep manually rather than via macros for now (one new
//! provider per quarter at most; macro overhead not worth it).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use tars_types::{Auth, HttpProviderExtras, ProviderId};

use crate::error::ValidationError;

/// Top-level providers section.
///
/// TOML shape:
/// ```toml
/// [providers.openai_main]
/// type = "openai"
/// auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
/// default_model = "gpt-4o"
///
/// [providers.local_qwen]
/// type = "openai_compat"
/// base_url = "http://localhost:8000/v1"
/// auth = { kind = "none" }
/// default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProvidersConfig {
    pub providers: HashMap<ProviderId, ProviderConfig>,
}

impl ProvidersConfig {
    pub fn from_map(map: HashMap<ProviderId, ProviderConfig>) -> Self {
        Self { providers: map }
    }
}

impl ProvidersConfig {
    pub fn iter(&self) -> impl Iterator<Item = (&ProviderId, &ProviderConfig)> {
        self.providers.iter()
    }

    pub fn get(&self, id: &ProviderId) -> Option<&ProviderConfig> {
        self.providers.get(id)
    }

    pub fn len(&self) -> usize {
        self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// One declarative provider entry. The serde tag is `type` to match the
/// TOML idiom (`type = "openai"`) — yes, "type" is a reserved word in
/// Rust source, but as a serde tag it's just a string.
///
/// HTTP-shape variants accept user-supplied `http_headers /
/// env_http_headers / query_params` fields (flattened into the variant
/// body). See [`HttpProviderExtras`] for semantics.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderConfig {
    /// Direct OpenAI HTTP API.
    Openai {
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        auth: Auth,
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
    },

    /// OpenAI-compatible HTTP server (Groq, Together, DeepSeek,
    /// llama.cpp server, LM Studio …). Distinguished from `Openai` only
    /// because `base_url` is mandatory here — keeps configs honest.
    OpenaiCompat {
        base_url: String,
        #[serde(default = "Auth::none")]
        auth: Auth,
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
    },

    /// Anthropic HTTP API.
    Anthropic {
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        api_version: Option<String>,
        auth: Auth,
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
    },

    /// Google Gemini HTTP API.
    Gemini {
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        auth: Auth,
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
    },

    /// vLLM local server (sub-case of openai_compat with sensible defaults).
    Vllm {
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default = "Auth::none")]
        auth: Auth,
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
    },

    /// Claude Code CLI subscription path.
    ClaudeCli {
        #[serde(default = "default_claude_executable")]
        executable: String,
        #[serde(default = "default_cli_timeout_secs")]
        timeout_secs: u64,
        default_model: String,
    },

    /// Gemini CLI subscription path.
    GeminiCli {
        #[serde(default = "default_gemini_executable")]
        executable: String,
        #[serde(default = "default_cli_timeout_secs")]
        timeout_secs: u64,
        default_model: String,
    },

    /// In-process mock — for tests and dry-run config validation.
    Mock {
        /// What the mock should reply with.
        #[serde(default = "default_mock_response")]
        canned_response: String,
    },
}

fn default_claude_executable() -> String {
    "claude".into()
}

fn default_gemini_executable() -> String {
    "gemini".into()
}

fn default_cli_timeout_secs() -> u64 {
    300
}

fn default_mock_response() -> String {
    "LGTM — no issues found.".into()
}

trait AuthDefaults {
    fn none() -> Auth;
}
impl AuthDefaults for Auth {
    fn none() -> Auth {
        Auth::None
    }
}

impl ProviderConfig {
    /// Diagnostic name for logs/audit.
    pub fn type_label(&self) -> &'static str {
        use ProviderConfig::*;
        match self {
            Openai { .. } => "openai",
            OpenaiCompat { .. } => "openai_compat",
            Anthropic { .. } => "anthropic",
            Gemini { .. } => "gemini",
            Vllm { .. } => "vllm",
            ClaudeCli { .. } => "claude_cli",
            GeminiCli { .. } => "gemini_cli",
            Mock { .. } => "mock",
        }
    }

    /// What model the provider defaults to. CLI providers and Mock
    /// always have one; Mock returns "mock-model".
    pub fn default_model(&self) -> &str {
        use ProviderConfig::*;
        match self {
            Openai { default_model, .. }
            | OpenaiCompat { default_model, .. }
            | Anthropic { default_model, .. }
            | Gemini { default_model, .. }
            | Vllm { default_model, .. }
            | ClaudeCli { default_model, .. }
            | GeminiCli { default_model, .. } => default_model,
            Mock { .. } => "mock-model",
        }
    }

    /// In-place validation that doesn't need any external state.
    /// Cross-provider checks (uniqueness, etc.) live on [`ProvidersConfig`].
    pub fn validate_self(&self, id: &ProviderId, sink: &mut Vec<ValidationError>) {
        let key = |k: &str| format!("providers.{id}.{k}");
        match self {
            ProviderConfig::OpenaiCompat { base_url, .. } if base_url.is_empty() => {
                sink.push(ValidationError::new(
                    key("base_url"),
                    "must be set for openai_compat (this distinguishes it from openai)",
                ));
            }
            ProviderConfig::Anthropic { auth, .. } => {
                if matches!(auth, Auth::None) {
                    sink.push(ValidationError::new(
                        key("auth"),
                        "Anthropic requires an api key — Auth::None is invalid",
                    ));
                }
            }
            ProviderConfig::Gemini { auth, .. } => {
                if matches!(auth, Auth::None) {
                    sink.push(ValidationError::new(
                        key("auth"),
                        "Gemini requires an api key — Auth::None is invalid",
                    ));
                }
            }
            ProviderConfig::ClaudeCli { executable, timeout_secs, default_model, .. } => {
                if executable.is_empty() {
                    sink.push(ValidationError::new(
                        key("executable"),
                        "must not be empty",
                    ));
                }
                if *timeout_secs == 0 {
                    sink.push(ValidationError::new(
                        key("timeout_secs"),
                        "must be > 0",
                    ));
                }
                if default_model.is_empty() {
                    sink.push(ValidationError::new(
                        key("default_model"),
                        "must not be empty",
                    ));
                }
            }
            ProviderConfig::GeminiCli { executable, timeout_secs, default_model, .. } => {
                if executable.is_empty() {
                    sink.push(ValidationError::new(key("executable"), "must not be empty"));
                }
                if *timeout_secs == 0 {
                    sink.push(ValidationError::new(key("timeout_secs"), "must be > 0"));
                }
                if default_model.is_empty() {
                    sink.push(ValidationError::new(
                        key("default_model"),
                        "must not be empty",
                    ));
                }
            }
            _ => {}
        }
    }
}

impl ProvidersConfig {
    /// Run [`ProviderConfig::validate_self`] over every entry, plus
    /// any cross-provider invariants.
    pub fn validate(&self, sink: &mut Vec<ValidationError>) {
        for (id, cfg) in &self.providers {
            cfg.validate_self(id, sink);
        }
        if self.is_empty() {
            sink.push(ValidationError::new(
                "providers",
                "no providers configured — at least one is required",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::SecretRef;

    #[test]
    fn openai_round_trips_through_toml() {
        let toml_str = r#"
            type = "openai"
            auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
            default_model = "gpt-4o"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match cfg {
            ProviderConfig::Openai { base_url, auth, default_model, extras: _ } => {
                assert!(base_url.is_none());
                assert_eq!(default_model, "gpt-4o");
                match auth {
                    Auth::Secret { secret: SecretRef::Env { var } } => {
                        assert_eq!(var, "OPENAI_API_KEY");
                    }
                    _ => panic!("wrong auth"),
                }
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn vllm_uses_default_auth_none_when_omitted() {
        let toml_str = r#"
            type = "vllm"
            default_model = "Qwen/Qwen2.5-Coder-7B-Instruct"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match cfg {
            ProviderConfig::Vllm { auth, .. } => {
                assert!(matches!(auth, Auth::None));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn claude_cli_defaults_executable_and_timeout() {
        let toml_str = r#"
            type = "claude_cli"
            default_model = "claude-opus-4-7"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match cfg {
            ProviderConfig::ClaudeCli { executable, timeout_secs, default_model } => {
                assert_eq!(executable, "claude");
                assert_eq!(timeout_secs, 300);
                assert_eq!(default_model, "claude-opus-4-7");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn validate_flags_anthropic_without_auth() {
        let cfg = ProviderConfig::Anthropic {
            base_url: None,
            api_version: None,
            auth: Auth::None,
            default_model: "claude-opus-4-7".into(),
            extras: HttpProviderExtras::default(),
        };
        let id = ProviderId::new("ant");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert!(errs.iter().any(|e| e.key.contains("auth")));
    }

    #[test]
    fn validate_flags_openai_compat_without_base_url() {
        let cfg = ProviderConfig::OpenaiCompat {
            base_url: String::new(),
            auth: Auth::None,
            default_model: "x".into(),
            extras: HttpProviderExtras::default(),
        };
        let id = ProviderId::new("compat");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert!(errs.iter().any(|e| e.key.contains("base_url")));
    }

    #[test]
    fn empty_providers_set_fails_validation() {
        let cfg = ProvidersConfig::default();
        let mut errs = Vec::new();
        cfg.validate(&mut errs);
        assert!(errs.iter().any(|e| e.key == "providers"));
    }

    #[test]
    fn type_label_round_trips() {
        let cfg = ProviderConfig::Mock { canned_response: "ok".into() };
        assert_eq!(cfg.type_label(), "mock");
    }

    #[test]
    fn full_providers_block_round_trips() {
        let toml_str = r#"
            [openai_main]
            type = "openai"
            auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
            default_model = "gpt-4o"

            [local_qwen]
            type = "openai_compat"
            base_url = "http://localhost:8000/v1"
            default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"

            [claude_cli]
            type = "claude_cli"
            default_model = "claude-opus-4-7"
        "#;
        let cfg: ProvidersConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.len(), 3);
        assert!(cfg.get(&ProviderId::new("openai_main")).is_some());
        assert!(cfg.get(&ProviderId::new("local_qwen")).is_some());
        assert!(cfg.get(&ProviderId::new("claude_cli")).is_some());
    }
}
