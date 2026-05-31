//! Provider declarations. The schema is **mirrored** from the concrete
//! backends in `tars-provider` — adding a new provider type means
//! adding a variant here AND a builder in `tars-provider`. We keep
//! them in lockstep manually rather than via macros for now (one new
//! provider per quarter at most; macro overhead not worth it).

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};

use tars_types::{Auth, HttpProviderExtras, ProviderId};

use crate::error::ValidationError;

/// Deserialize a `String` field, trimming surrounding whitespace.
///
/// Normalising at the parse boundary keeps the stored value identical to
/// what `validate()`'s `trim().is_empty()` checks against and to what the
/// runtime (e.g. the HTTP client `base_url`, the CLI `executable`) actually
/// uses — otherwise a value like `"  claude  "` would pass validation yet
/// reach the runtime with its whitespace intact.
fn de_trimmed_string<'de, D>(de: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(de)?;
    Ok(s.trim().to_owned())
}

/// `Option<String>` counterpart of [`de_trimmed_string`]. A present value is
/// trimmed; absence stays `None`. An all-whitespace value trims to `Some("")`
/// and is then rejected by `validate()` (the "empty when set" contract).
fn de_trimmed_opt_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(de)?;
    Ok(opt.map(|s| s.trim().to_owned()))
}

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
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        base_url: Option<String>,
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
    },

    /// OpenAI-compatible HTTP server (Groq, Together, DeepSeek,
    /// llama.cpp server, LM Studio …). Distinguished from `Openai` only
    /// because `base_url` is mandatory here — keeps configs honest.
    OpenaiCompat {
        #[serde(deserialize_with = "de_trimmed_string")]
        base_url: String,
        #[serde(default = "Auth::none")]
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
        #[serde(flatten)]
        capabilities: CapabilitiesOverrides,
    },

    /// Anthropic HTTP API.
    Anthropic {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        base_url: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        api_version: Option<String>,
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
    },

    /// Google Gemini HTTP API.
    Gemini {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        base_url: Option<String>,
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
    },

    /// vLLM local server (sub-case of openai_compat with sensible defaults).
    Vllm {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        base_url: Option<String>,
        #[serde(default = "Auth::none")]
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
        #[serde(flatten)]
        capabilities: CapabilitiesOverrides,
    },

    /// Apple Silicon native runner (`mlx_lm.server`). Same wire format
    /// as vLLM/llama.cpp/OpenAI-compat — the dedicated variant exists
    /// so logs and routing can identify "this is the unified-memory
    /// box" at a glance and apply Mac-specific capability defaults.
    Mlx {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        base_url: Option<String>,
        #[serde(default = "Auth::none")]
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
        #[serde(flatten)]
        capabilities: CapabilitiesOverrides,
    },

    /// llama.cpp `llama-server` (GGUF + Vulkan/Metal). Use this for
    /// Ryzen iGPU clusters or any host where `llama.cpp` is the
    /// preferred runner. Same wire format as the other local backends.
    Llamacpp {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        base_url: Option<String>,
        #[serde(default = "Auth::none")]
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        #[serde(flatten)]
        extras: HttpProviderExtras,
        #[serde(flatten)]
        capabilities: CapabilitiesOverrides,
    },

    /// Claude Code CLI subscription path.
    ClaudeCli {
        #[serde(
            default = "default_claude_executable",
            deserialize_with = "de_trimmed_string"
        )]
        executable: String,
        #[serde(default = "default_cli_timeout_secs")]
        timeout_secs: u64,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        /// `--tools` knob. `"disabled"` (default) kills the CLI's
        /// internal agent loop while staying auth-neutral; `"default"`
        /// preserves full agent behavior; an array names specific
        /// allowed tools. See `docs/providers/claude-cli.md §3`.
        #[serde(default)]
        tools: ClaudeCliToolsConfig,
        /// `--bare`. **Default `false`** — setting `true` skips
        /// auto-memory / CLAUDE.md auto-discovery **and disables
        /// OAuth/keychain auth**, requiring `ANTHROPIC_API_KEY`.
        /// Most `claude_cli` users log in via `claude login` and would
        /// break with `bare=true`. See `docs/providers/claude-cli.md §3`.
        #[serde(default)]
        bare: bool,
        /// `--effort` level. `None` (default) omits the flag.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<ClaudeCliEffortConfig>,
        /// `--exclude-dynamic-system-prompt-sections`. Default `true`
        /// (improves prompt-cache reuse).
        #[serde(default = "default_true")]
        exclude_dynamic_sections: bool,
        /// Escape hatch — raw argv tokens appended to every call.
        /// Use sparingly; the Builder methods cover the common cases.
        #[serde(default)]
        extra_args: Vec<String>,
    },

    /// Gemini CLI subscription path.
    GeminiCli {
        #[serde(
            default = "default_gemini_executable",
            deserialize_with = "de_trimmed_string"
        )]
        executable: String,
        #[serde(default = "default_cli_timeout_secs")]
        timeout_secs: u64,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
    },

    /// Claude SDK child process — tars spawns a long-lived
    /// `@anthropic-ai/claude-agent-sdk` Node process and multiplexes
    /// requests over its stdin / stdout (NDJSON, one JSON object per
    /// line). The child owns subscription OAuth and prompt-cache
    /// lifetime; tars owns the pipe.
    ///
    /// Eliminates the per-call cold start of [`Self::ClaudeCli`]
    /// (which spawns a fresh `claude` subprocess per request) while
    /// avoiding the operator burden of [`Self::ClaudeCli`]'s
    /// alternative — a long-running HTTP daemon — by tying child
    /// lifetime to tars's. See
    /// `crates/tars-provider/src/backends/claude_sdk.rs` for the
    /// transport, and `tools/claude-daemon/server.mjs --stdio` for
    /// the reference child script.
    ClaudeSdk {
        /// Node-compatible runtime that hosts the SDK script. Default
        /// `node`. Override e.g. to `bun` or a pinned path.
        #[serde(
            default = "default_node_executable",
            deserialize_with = "de_trimmed_string"
        )]
        executable: String,
        /// Path to `server.mjs --stdio`. **Optional** — when unset
        /// (the common case), the backend searches in order:
        /// 1. `TARS_CLAUDE_SDK_SCRIPT` env var
        /// 2. `tools/claude-daemon/server.mjs` walking up from the CWD
        ///    (works inside the tars checkout)
        /// 3. `~/.tars/claude-daemon/server.mjs` (standard install)
        ///
        /// Set this explicitly to pin a specific copy or to use a
        /// non-standard layout.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        script_path: Option<String>,
        /// Round-trip cap on a single LLM call. The child stays warm
        /// across calls; this is only how long we wait for any one
        /// reply before declaring it lost. Default 300s parallels
        /// other CLI-shaped backends.
        #[serde(default = "default_cli_timeout_secs")]
        timeout_secs: u64,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
    },

    /// OpenAI Codex CLI subscription path (ChatGPT Plus/Pro).
    CodexCli {
        #[serde(
            default = "default_codex_executable",
            deserialize_with = "de_trimmed_string"
        )]
        executable: String,
        /// Per-call timeout. codex `exec` runs a full agent loop;
        /// default is more generous than other CLIs (10 min vs 5).
        #[serde(default = "default_codex_timeout_secs")]
        timeout_secs: u64,
        /// Sandbox mode for codex's INTERNAL tools (its sandbox-shell,
        /// apply-patch, etc. — not TARS's tool registry). Default
        /// `read-only` keeps the principle of least surprise: a TARS
        /// Worker shouldn't get unexpected file mutations.
        #[serde(default)]
        sandbox: CodexSandboxConfig,
        /// Pass `--skip-git-repo-check` (default true). TARS Workers
        /// often run outside a git repo (tempdir tests, scratch
        /// files); codex's git-repo gate would reject them with
        /// confusing wording.
        #[serde(default = "default_true")]
        skip_git_repo_check: bool,
        #[serde(deserialize_with = "de_trimmed_string")]
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

fn default_codex_executable() -> String {
    "codex".into()
}

fn default_cli_timeout_secs() -> u64 {
    300
}

fn default_codex_timeout_secs() -> u64 {
    600
}

fn default_node_executable() -> String {
    "node".into()
}

fn default_true() -> bool {
    true
}

/// TOML-friendly mirror of [`tars_provider::backends::codex_cli::SandboxMode`].
/// Lives here (not in provider) because config is the canonical wire shape;
/// the provider crate's enum is the runtime equivalent and the registry
/// builder bridges between them.
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSandboxConfig {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// TOML-friendly mirror of [`tars_provider::ClaudeCliTools`].
///
/// The two string variants (`"disabled"` / `"default"`) cover the
/// common cases; the list form lets users hand-pick a tool whitelist.
/// `#[serde(untagged)]` so users can write either shape:
///
/// ```toml
/// tools = "disabled"
/// tools = "default"
/// tools = ["Read", "Bash"]
/// ```
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ClaudeCliToolsConfig {
    /// `"disabled"` or `"default"`.
    Named(ClaudeCliToolsKeyword),
    /// Explicit allow-list, e.g. `["Read", "Bash"]`.
    Allow(Vec<String>),
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeCliToolsKeyword {
    Disabled,
    Default,
}

impl Default for ClaudeCliToolsConfig {
    fn default() -> Self {
        Self::Named(ClaudeCliToolsKeyword::Disabled)
    }
}

/// TOML-friendly mirror of [`tars_provider::ClaudeCliEffort`].
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeCliEffortConfig {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

fn default_mock_response() -> String {
    "LGTM — no issues found.".into()
}

/// Optional per-provider overrides for the backend's hardcoded
/// [`tars_types::Capabilities`]. Today fields are limited to the
/// two numbers users actually need to tune (context + output cap);
/// the boolean `supports_*` flags can be added later when a real
/// consumer needs to flip them via TOML (e.g. "this self-hosted
/// vLLM lies about strict-output support").
///
/// Empty struct means "use backend defaults". Apply at registry
/// build time:
///
/// ```toml
/// [providers.mlx_local]
/// type = "mlx"
/// default_model = "..."
/// max_context_tokens = 262144   # Qwen3 supports 256K — bump from 32K default
/// max_output_tokens = 32768     # bump from 4K default
/// ```
///
/// Thaws Doc 01 D-6 (`capabilities_override` config field — the
/// trigger fired the first time a user with a heterogeneous local
/// deployment hit our default 4K output cap).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CapabilitiesOverrides {
    /// Override the maximum prompt-context-window the model accepts.
    /// `None` = keep backend default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u32>,
    /// Override the maximum tokens the caller may request as output.
    /// `None` = keep backend default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

impl CapabilitiesOverrides {
    /// True iff every override field is `None` (no overrides supplied).
    pub fn is_empty(&self) -> bool {
        self.max_context_tokens.is_none() && self.max_output_tokens.is_none()
    }

    /// Apply overrides onto a hardcoded backend default.
    pub fn apply_to(&self, base: &mut tars_types::Capabilities) {
        if let Some(n) = self.max_context_tokens {
            base.max_context_tokens = n;
        }
        if let Some(n) = self.max_output_tokens {
            base.max_output_tokens = n;
        }
    }
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
            | Mlx { default_model, .. }
            | Llamacpp { default_model, .. }
            | ClaudeCli { default_model, .. }
            | ClaudeSdk { default_model, .. }
            | GeminiCli { default_model, .. }
            | CodexCli { default_model, .. } => default_model,
            Mock { .. } => "mock-model",
        }
    }

    /// In-place validation that doesn't need any external state.
    /// Cross-provider checks (uniqueness, etc.) live on [`ProvidersConfig`].
    pub fn validate_self(&self, id: &ProviderId, sink: &mut Vec<ValidationError>) {
        let key = |k: &str| format!("providers.{id}.{k}");

        // Helpers — every provider needs these checks, so factor them out
        // rather than open-code each variant arm.
        let check_default_model = |dm: &str, sink: &mut Vec<ValidationError>| {
            // `trim()` so a whitespace-only value (`"   "`) is rejected too —
            // it would pass a bare `is_empty()` check but fail at runtime
            // when the provider tries to use it. Matches the base_url checks.
            if dm.trim().is_empty() {
                sink.push(ValidationError::new(
                    key("default_model"),
                    "must not be empty",
                ));
            }
        };
        let check_opt_base_url = |opt: &Option<String>, sink: &mut Vec<ValidationError>| {
            if matches!(opt, Some(s) if s.trim().is_empty()) {
                sink.push(ValidationError::new(
                    key("base_url"),
                    "must not be empty when set (omit the field to use the default)",
                ));
            }
        };
        // Reject explicitly-set zero token caps: a model can't have a
        // zero context window or zero output budget, and a `Some(0)`
        // would silently corrupt the runtime `Capabilities` via
        // `apply_to`. `None` (no override) stays valid.
        let check_capabilities = |caps: &CapabilitiesOverrides, sink: &mut Vec<ValidationError>| {
            if matches!(caps.max_context_tokens, Some(0)) {
                sink.push(ValidationError::new(
                    key("max_context_tokens"),
                    "must be > 0 when set",
                ));
            }
            if matches!(caps.max_output_tokens, Some(0)) {
                sink.push(ValidationError::new(
                    key("max_output_tokens"),
                    "must be > 0 when set",
                ));
            }
        };
        let check_timeout = |secs: u64, sink: &mut Vec<ValidationError>| {
            if secs == 0 {
                sink.push(ValidationError::new(key("timeout_secs"), "must be > 0"));
            } else if secs > MAX_CLI_TIMEOUT_SECS {
                sink.push(ValidationError::new(
                    key("timeout_secs"),
                    format!(
                        "must be <= {MAX_CLI_TIMEOUT_SECS} (24h); larger values are almost \
                         certainly a typo — a per-call timeout beyond a day is never intended"
                    ),
                ));
            }
        };

        match self {
            ProviderConfig::Openai {
                base_url,
                auth,
                default_model,
                ..
            } => {
                check_opt_base_url(base_url, sink);
                if matches!(auth, Auth::None) {
                    sink.push(ValidationError::new(
                        key("auth"),
                        "OpenAI requires an api key — Auth::None is invalid \
                         (use openai_compat for keyless local/proxy endpoints)",
                    ));
                }
                check_default_model(default_model, sink);
            }
            ProviderConfig::OpenaiCompat {
                base_url,
                default_model,
                capabilities,
                ..
            } => {
                if base_url.trim().is_empty() {
                    sink.push(ValidationError::new(
                        key("base_url"),
                        "must be set for openai_compat (this distinguishes it from openai)",
                    ));
                }
                check_default_model(default_model, sink);
                check_capabilities(capabilities, sink);
            }
            ProviderConfig::Anthropic {
                base_url,
                api_version,
                auth,
                default_model,
                ..
            } => {
                check_opt_base_url(base_url, sink);
                // Same empty-when-set contract as base_url: a `Some("")`
                // or `Some("   ")` would pass an unchecked `..` and then
                // surface as a confusing wire error against the backend.
                if matches!(api_version, Some(s) if s.trim().is_empty()) {
                    sink.push(ValidationError::new(
                        key("api_version"),
                        "must not be empty when set (omit the field to use the default)",
                    ));
                }
                if matches!(auth, Auth::None) {
                    sink.push(ValidationError::new(
                        key("auth"),
                        "Anthropic requires an api key — Auth::None is invalid",
                    ));
                }
                check_default_model(default_model, sink);
            }
            ProviderConfig::Gemini {
                base_url,
                auth,
                default_model,
                ..
            } => {
                check_opt_base_url(base_url, sink);
                if matches!(auth, Auth::None) {
                    sink.push(ValidationError::new(
                        key("auth"),
                        "Gemini requires an api key — Auth::None is invalid",
                    ));
                }
                check_default_model(default_model, sink);
            }
            ProviderConfig::Vllm {
                base_url,
                default_model,
                capabilities,
                ..
            }
            | ProviderConfig::Mlx {
                base_url,
                default_model,
                capabilities,
                ..
            }
            | ProviderConfig::Llamacpp {
                base_url,
                default_model,
                capabilities,
                ..
            } => {
                check_opt_base_url(base_url, sink);
                check_default_model(default_model, sink);
                check_capabilities(capabilities, sink);
            }
            ProviderConfig::ClaudeCli {
                executable,
                timeout_secs,
                default_model,
                ..
            }
            | ProviderConfig::GeminiCli {
                executable,
                timeout_secs,
                default_model,
            } => {
                if executable.trim().is_empty() {
                    sink.push(ValidationError::new(key("executable"), "must not be empty"));
                }
                check_timeout(*timeout_secs, sink);
                check_default_model(default_model, sink);
            }
            ProviderConfig::CodexCli {
                executable,
                timeout_secs,
                default_model,
                ..
            } => {
                if executable.trim().is_empty() {
                    sink.push(ValidationError::new(key("executable"), "must not be empty"));
                }
                check_timeout(*timeout_secs, sink);
                check_default_model(default_model, sink);
            }
            ProviderConfig::ClaudeSdk {
                executable,
                script_path,
                timeout_secs,
                default_model,
            } => {
                if executable.trim().is_empty() {
                    sink.push(ValidationError::new(key("executable"), "must not be empty"));
                }
                // script_path is optional — when None, the backend
                // searches CWD ancestors and ~/.tars/. We only reject
                // an *empty* string here (someone wrote `""` instead of
                // omitting the field), which would otherwise short-
                // circuit the fallback search at runtime.
                if let Some(sp) = script_path {
                    if sp.trim().is_empty() {
                        sink.push(ValidationError::new(
                            key("script_path"),
                            "must not be empty — omit the field to use the default search instead",
                        ));
                    }
                }
                check_timeout(*timeout_secs, sink);
                check_default_model(default_model, sink);
            }
            ProviderConfig::Mock { .. } => {}
        }
    }
}

/// Upper bound on CLI timeouts — 24h. Anything beyond this is almost
/// certainly a typo: a per-call timeout longer than a day is never a
/// real intent. (The value is passed straight to
/// `tokio::time::timeout(Duration::from_secs(_), …)` with no arithmetic,
/// so this cap is a sanity guard, not an overflow guard.)
const MAX_CLI_TIMEOUT_SECS: u64 = 86_400;

impl ProvidersConfig {
    /// Run [`ProviderConfig::validate_self`] over every entry.
    ///
    /// We don't check "non-empty" here because the loader merges
    /// built-in defaults under whatever the user declared (see
    /// `manager::merge_builtins_into`), so the post-merge providers
    /// table is never empty in practice. A user-level "you wrote
    /// nothing useful" warning, if anyone wants one, belongs at a
    /// higher layer that knows what the caller is trying to do.
    pub fn validate(&self, sink: &mut Vec<ValidationError>) {
        for (id, cfg) in &self.providers {
            cfg.validate_self(id, sink);
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
        // Forward direction.
        match &cfg {
            ProviderConfig::Openai {
                base_url,
                auth,
                default_model,
                extras: _,
            } => {
                assert!(base_url.is_none());
                assert_eq!(default_model, "gpt-4o");
                match auth {
                    Auth::Secret {
                        secret: SecretRef::Env { var },
                    } => {
                        assert_eq!(var, "OPENAI_API_KEY");
                    }
                    _ => panic!("wrong auth"),
                }
            }
            _ => panic!("wrong variant"),
        }
        // Reverse direction: serialize the parsed value and re-parse it,
        // confirming the wire shape is bidirectionally stable.
        let reserialized = toml::to_string(&cfg).expect("must serialize");
        let cfg2: ProviderConfig =
            toml::from_str(&reserialized).expect("re-parse after serialize must succeed");
        match cfg2 {
            ProviderConfig::Openai {
                auth,
                default_model,
                ..
            } => {
                assert_eq!(default_model, "gpt-4o");
                assert!(matches!(
                    auth,
                    Auth::Secret { secret: SecretRef::Env { var } } if var == "OPENAI_API_KEY"
                ));
            }
            _ => panic!("re-parsed variant changed"),
        }
    }

    #[test]
    fn mlx_uses_default_auth_none_when_omitted() {
        let toml_str = r#"
            type = "mlx"
            default_model = "mlx-community/Qwen2.5-Coder-32B-Instruct-4bit"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match cfg {
            ProviderConfig::Mlx {
                auth,
                default_model,
                ..
            } => {
                assert!(matches!(auth, Auth::None));
                assert!(default_model.contains("Qwen"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn llamacpp_uses_default_auth_none_when_omitted() {
        let toml_str = r#"
            type = "llamacpp"
            default_model = "Qwen2.5-Coder-7B-Q5_K_M"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match cfg {
            ProviderConfig::Llamacpp { auth, .. } => {
                assert!(matches!(auth, Auth::None));
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
            ProviderConfig::ClaudeCli {
                executable,
                timeout_secs,
                default_model,
                ..
            } => {
                assert_eq!(executable, "claude");
                assert_eq!(timeout_secs, 300);
                assert_eq!(default_model, "claude-opus-4-7");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn mlx_capability_overrides_round_trip_through_toml() {
        // The whole reason this struct exists: a user with a 256K-context
        // model needs to bump the default 32K. Pin that this works.
        let toml_str = r#"
            type = "mlx"
            default_model = "mlx-community/Qwen3-32B-256K-mlx-4bit"
            max_context_tokens = 262144
            max_output_tokens = 32768
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match cfg {
            ProviderConfig::Mlx { capabilities, .. } => {
                assert_eq!(capabilities.max_context_tokens, Some(262144));
                assert_eq!(capabilities.max_output_tokens, Some(32768));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn mlx_without_capability_overrides_serializes_clean() {
        // No overrides → empty struct → no extra TOML keys emitted.
        let toml_str = r#"
            type = "mlx"
            default_model = "..."
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match &cfg {
            ProviderConfig::Mlx { capabilities, .. } => {
                assert!(capabilities.is_empty());
            }
            _ => panic!("wrong variant"),
        }
        // Round-trip: serialize, no `max_*_tokens` keys.
        let back = toml::to_string(&cfg).unwrap();
        assert!(
            !back.contains("max_context_tokens"),
            "empty overrides must not emit the field; got: {back}",
        );
    }

    #[test]
    fn capabilities_overrides_apply_to_replaces_only_set_fields() {
        let mut base = tars_types::Capabilities::text_only_baseline(tars_types::Pricing::default());
        base.max_context_tokens = 4096;
        base.max_output_tokens = 1024;

        // Override only context; output stays at base.
        let overrides = CapabilitiesOverrides {
            max_context_tokens: Some(262144),
            max_output_tokens: None,
        };
        overrides.apply_to(&mut base);
        assert_eq!(base.max_context_tokens, 262144);
        assert_eq!(base.max_output_tokens, 1024);
    }

    #[test]
    fn codex_cli_defaults_executable_timeout_sandbox_and_git_check() {
        let toml_str = r#"
            type = "codex_cli"
            default_model = "gpt-5"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match cfg {
            ProviderConfig::CodexCli {
                executable,
                timeout_secs,
                sandbox,
                skip_git_repo_check,
                default_model,
            } => {
                assert_eq!(executable, "codex");
                assert_eq!(timeout_secs, 600); // codex gets 10min default vs other CLIs' 5
                assert_eq!(sandbox, CodexSandboxConfig::ReadOnly);
                assert!(skip_git_repo_check);
                assert_eq!(default_model, "gpt-5");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn codex_cli_sandbox_kebab_case_round_trips() {
        // Pin the wire-format so a future renames-by-mistake breaks
        // loudly rather than silently downgrading sandbox.
        let toml_str = r#"
            type = "codex_cli"
            default_model = "gpt-5"
            sandbox = "workspace-write"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match &cfg {
            ProviderConfig::CodexCli { sandbox, .. } => {
                assert_eq!(*sandbox, CodexSandboxConfig::WorkspaceWrite);
            }
            _ => panic!("wrong variant"),
        }
        // Reverse direction: serialize and confirm we still emit the
        // kebab-case spelling, then re-parse to confirm full bidirectional
        // stability.
        let reserialized = toml::to_string(&cfg).expect("must serialize");
        assert!(
            reserialized.contains("workspace-write"),
            "expected kebab-case 'workspace-write' in serialized output, got: {reserialized}"
        );
        let cfg2: ProviderConfig =
            toml::from_str(&reserialized).expect("re-parse after serialize must succeed");
        match cfg2 {
            ProviderConfig::CodexCli { sandbox, .. } => {
                assert_eq!(sandbox, CodexSandboxConfig::WorkspaceWrite);
            }
            _ => panic!("re-parsed variant changed"),
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
        assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
        assert_eq!(errs[0].key, "providers.ant.auth");
        assert!(
            errs[0].message.contains("api key"),
            "message should mention api key, got: {}",
            errs[0].message
        );
    }

    #[test]
    fn validate_flags_gemini_without_auth() {
        let cfg = ProviderConfig::Gemini {
            base_url: None,
            auth: Auth::None,
            default_model: "gemini-2.5-pro".into(),
            extras: HttpProviderExtras::default(),
        };
        let id = ProviderId::new("gem");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
        assert_eq!(errs[0].key, "providers.gem.auth");
        assert!(
            errs[0].message.contains("api key"),
            "message should mention api key, got: {}",
            errs[0].message
        );
    }

    #[test]
    fn validate_flags_openai_without_auth() {
        // Mirror Anthropic/Gemini: OpenAI's real API requires an api
        // key; users wanting keyless local/proxy endpoints should use
        // `openai_compat`.
        let cfg = ProviderConfig::Openai {
            base_url: None,
            auth: Auth::None,
            default_model: "gpt-4o".into(),
            extras: HttpProviderExtras::default(),
        };
        let id = ProviderId::new("oa");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
        assert_eq!(errs[0].key, "providers.oa.auth");
    }

    #[test]
    fn validate_flags_openai_compat_without_base_url() {
        let cfg = ProviderConfig::OpenaiCompat {
            base_url: String::new(),
            auth: Auth::None,
            default_model: "x".into(),
            extras: HttpProviderExtras::default(),
            capabilities: Default::default(),
        };
        let id = ProviderId::new("compat");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
        assert_eq!(errs[0].key, "providers.compat.base_url");
    }

    #[test]
    fn validate_flags_http_provider_with_empty_default_model() {
        // CLI providers explicitly check this; HTTP providers must too.
        let cfg = ProviderConfig::Anthropic {
            base_url: None,
            api_version: None,
            auth: Auth::env("ANTHROPIC_API_KEY"),
            default_model: String::new(),
            extras: HttpProviderExtras::default(),
        };
        let id = ProviderId::new("ant");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].key, "providers.ant.default_model");
    }

    #[test]
    fn validate_flags_optional_base_url_set_to_empty_string() {
        let cfg = ProviderConfig::Vllm {
            base_url: Some(String::new()),
            auth: Auth::None,
            default_model: "model".into(),
            extras: HttpProviderExtras::default(),
            capabilities: Default::default(),
        };
        let id = ProviderId::new("v");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].key, "providers.v.base_url");
    }

    #[test]
    fn validate_flags_cli_timeout_above_24h_cap() {
        let cfg = ProviderConfig::ClaudeCli {
            executable: "claude".into(),
            timeout_secs: 86_401,
            default_model: "claude-opus-4-7".into(),
            tools: ClaudeCliToolsConfig::default(),
            bare: false,
            effort: None,
            exclude_dynamic_sections: true,
            extra_args: Vec::new(),
        };
        let id = ProviderId::new("c");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].key, "providers.c.timeout_secs");
    }

    #[test]
    fn validate_accepts_well_formed_configs() {
        // Positive case: a fully-valid Anthropic + valid OpenAI + valid
        // OpenaiCompat + valid CLI should produce zero errors. Catches
        // the "validation accidentally rejects valid configs" regression.
        let id = ProviderId::new("good");
        let mut errs = Vec::new();

        ProviderConfig::Anthropic {
            base_url: None,
            api_version: None,
            auth: Auth::env("ANTHROPIC_API_KEY"),
            default_model: "claude-opus-4-7".into(),
            extras: HttpProviderExtras::default(),
        }
        .validate_self(&id, &mut errs);

        ProviderConfig::Openai {
            base_url: None,
            auth: Auth::env("OPENAI_API_KEY"),
            default_model: "gpt-4o".into(),
            extras: HttpProviderExtras::default(),
        }
        .validate_self(&id, &mut errs);

        ProviderConfig::OpenaiCompat {
            base_url: "http://localhost:8000/v1".into(),
            auth: Auth::None,
            default_model: "Qwen/Qwen2.5-Coder-32B-Instruct".into(),
            extras: HttpProviderExtras::default(),
            capabilities: Default::default(),
        }
        .validate_self(&id, &mut errs);

        ProviderConfig::ClaudeCli {
            executable: "claude".into(),
            timeout_secs: 300,
            default_model: "claude-opus-4-7".into(),
            tools: ClaudeCliToolsConfig::default(),
            bare: false,
            effort: None,
            exclude_dynamic_sections: true,
            extra_args: Vec::new(),
        }
        .validate_self(&id, &mut errs);

        assert!(errs.is_empty(), "valid configs produced errors: {errs:?}");
    }

    #[test]
    fn empty_providers_set_validates_clean() {
        // Per Stage-2 change: empty providers no longer raises a
        // standalone validation error here. The loader merges built-in
        // defaults under the user table before validation runs, so
        // post-merge the providers map always has entries. This unit
        // test exercises raw `validate()` on an empty map (i.e. the
        // pre-merge case is no longer flagged by this layer).
        let cfg = ProvidersConfig::default();
        let mut errs = Vec::new();
        cfg.validate(&mut errs);
        assert!(errs.is_empty(), "expected no errors, got: {errs:?}");
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

        // Field-level assertions, not just .is_some(), so a regression
        // that swaps fields between providers fails loudly.
        match cfg
            .get(&ProviderId::new("openai_main"))
            .expect("openai_main missing")
        {
            ProviderConfig::Openai {
                base_url,
                auth,
                default_model,
                ..
            } => {
                assert!(base_url.is_none());
                assert_eq!(default_model, "gpt-4o");
                assert!(matches!(
                    auth,
                    Auth::Secret { secret: SecretRef::Env { var } } if var == "OPENAI_API_KEY"
                ));
            }
            other => panic!("openai_main wrong variant: {other:?}"),
        }
        match cfg
            .get(&ProviderId::new("local_qwen"))
            .expect("local_qwen missing")
        {
            ProviderConfig::OpenaiCompat {
                base_url,
                default_model,
                ..
            } => {
                assert_eq!(base_url, "http://localhost:8000/v1");
                assert_eq!(default_model, "Qwen/Qwen2.5-Coder-32B-Instruct");
            }
            other => panic!("local_qwen wrong variant: {other:?}"),
        }
        match cfg
            .get(&ProviderId::new("claude_cli"))
            .expect("claude_cli missing")
        {
            ProviderConfig::ClaudeCli {
                default_model,
                executable,
                timeout_secs,
                ..
            } => {
                assert_eq!(default_model, "claude-opus-4-7");
                assert_eq!(executable, "claude"); // default
                assert_eq!(*timeout_secs, 300); // default
            }
            other => panic!("claude_cli wrong variant: {other:?}"),
        }

        // Reverse direction: serialize the parsed config and re-parse,
        // confirming the providers block is bidirectionally stable.
        let reserialized = toml::to_string(&cfg).expect("must serialize");
        let cfg2: ProvidersConfig =
            toml::from_str(&reserialized).expect("re-parse after serialize must succeed");
        assert_eq!(cfg2.len(), 3);
        assert!(matches!(
            cfg2.get(&ProviderId::new("openai_main")),
            Some(ProviderConfig::Openai { .. })
        ));
        assert!(matches!(
            cfg2.get(&ProviderId::new("local_qwen")),
            Some(ProviderConfig::OpenaiCompat { .. })
        ));
        assert!(matches!(
            cfg2.get(&ProviderId::new("claude_cli")),
            Some(ProviderConfig::ClaudeCli { .. })
        ));
    }

    #[test]
    fn claude_cli_omitted_fields_use_safe_defaults() {
        // The only thing the user MUST provide is `default_model`. Every
        // other knob should fall back to the subscription-friendly
        // defaults the architecture doc and Builder agreed on.
        let toml_str = r#"
            [claude_cli]
            type = "claude_cli"
            default_model = "claude-sonnet-4-5"
        "#;
        let cfg: ProvidersConfig = toml::from_str(toml_str).expect("parse");
        let p = cfg.get(&ProviderId::new("claude_cli")).expect("entry");
        match p {
            ProviderConfig::ClaudeCli {
                tools,
                bare,
                effort,
                exclude_dynamic_sections,
                extra_args,
                ..
            } => {
                assert_eq!(
                    tools,
                    &ClaudeCliToolsConfig::Named(ClaudeCliToolsKeyword::Disabled),
                    "tools default must be Disabled — kills agent loop, auth-neutral"
                );
                assert!(
                    !bare,
                    "bare default must be false — preserves OAuth/keychain auth"
                );
                assert!(effort.is_none(), "effort default must be unset");
                assert!(
                    exclude_dynamic_sections,
                    "exclude_dynamic_sections default must be true (cache reuse)"
                );
                assert!(extra_args.is_empty());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn claude_cli_tools_accepts_all_three_shapes() {
        // Keyword "disabled".
        let cfg: ProvidersConfig = toml::from_str(
            r#"
            [c]
            type = "claude_cli"
            default_model = "m"
            tools = "disabled"
            "#,
        )
        .unwrap();
        match cfg.get(&ProviderId::new("c")).unwrap() {
            ProviderConfig::ClaudeCli { tools, .. } => {
                assert_eq!(
                    tools,
                    &ClaudeCliToolsConfig::Named(ClaudeCliToolsKeyword::Disabled)
                );
            }
            _ => panic!(),
        }

        // Keyword "default".
        let cfg: ProvidersConfig = toml::from_str(
            r#"
            [c]
            type = "claude_cli"
            default_model = "m"
            tools = "default"
            "#,
        )
        .unwrap();
        match cfg.get(&ProviderId::new("c")).unwrap() {
            ProviderConfig::ClaudeCli { tools, .. } => {
                assert_eq!(
                    tools,
                    &ClaudeCliToolsConfig::Named(ClaudeCliToolsKeyword::Default)
                );
            }
            _ => panic!(),
        }

        // List of tool names.
        let cfg: ProvidersConfig = toml::from_str(
            r#"
            [c]
            type = "claude_cli"
            default_model = "m"
            tools = ["Read", "Bash"]
            "#,
        )
        .unwrap();
        match cfg.get(&ProviderId::new("c")).unwrap() {
            ProviderConfig::ClaudeCli { tools, .. } => {
                assert_eq!(
                    tools,
                    &ClaudeCliToolsConfig::Allow(vec!["Read".into(), "Bash".into()])
                );
            }
            _ => panic!(),
        }
    }

    #[test]
    fn claude_cli_full_config_round_trips() {
        let toml_in = r#"
            [c]
            type = "claude_cli"
            default_model = "claude-sonnet-4-5"
            executable = "claude"
            timeout_secs = 180
            tools = ["Read", "Bash(git *)"]
            bare = true
            effort = "high"
            exclude_dynamic_sections = false
            extra_args = ["--betas", "experimental-x"]
        "#;
        let cfg: ProvidersConfig = toml::from_str(toml_in).expect("parse");
        let reserialized = toml::to_string(&cfg).expect("serialize");
        let cfg2: ProvidersConfig =
            toml::from_str(&reserialized).expect("re-parse after serialize");
        match cfg2.get(&ProviderId::new("c")).unwrap() {
            ProviderConfig::ClaudeCli {
                executable,
                timeout_secs,
                default_model,
                tools,
                bare,
                effort,
                exclude_dynamic_sections,
                extra_args,
            } => {
                assert_eq!(executable, "claude");
                assert_eq!(*timeout_secs, 180);
                assert_eq!(default_model, "claude-sonnet-4-5");
                assert_eq!(
                    tools,
                    &ClaudeCliToolsConfig::Allow(vec!["Read".into(), "Bash(git *)".into()])
                );
                assert!(bare);
                assert_eq!(effort, &Some(ClaudeCliEffortConfig::High));
                assert!(!exclude_dynamic_sections);
                assert_eq!(
                    extra_args,
                    &vec!["--betas".to_string(), "experimental-x".into()]
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
