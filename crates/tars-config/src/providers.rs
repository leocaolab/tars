//! Declares the config schema of each provider type and checks each entry on its own.
//!
//! Every variant has a builder in `tars-llm`'s registry, kept in step by hand: a new provider
//! type is a variant here and a builder there.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};

use tars_types::{Auth, HttpOverrides, ProviderId};

use crate::error::{ValidationError, ValidationProblem};

/// Deserializes a `String` with surrounding whitespace trimmed, so the value `validate` checks is
/// the one the runtime uses; `"  claude  "` would otherwise pass and reach the spawn as is.
fn de_trimmed_string<'de, D>(de: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(de)?;
    Ok(s.trim().to_owned())
}

/// Deserializes an optional `String` trimmed; an all-whitespace value becomes `Some("")`, which
/// `validate` rejects.
fn de_trimmed_opt_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(de)?;
    Ok(opt.map(|s| s.trim().to_owned()))
}

/// The `[providers]` table, keyed by provider id.
///
/// `default_model` takes a concrete model id, or `<series>@latest` for the newest model of a
/// series the provider defines in `data/provider.toml` (Gemini: `flash`, `flash-lite`, `pro`);
/// see [`crate::model_spec`].
///
/// ```toml
/// [providers.openai_main]
/// type = "openai"
/// auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
/// default_model = "gpt-5.4"
///
/// [providers.gemini_flash]
/// type = "gemini"
/// auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
/// default_model = "flash@latest"
///
/// [providers.local_qwen]
/// type = "openai_compat"
/// gen_ai_provider_name = "vllm"
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

/// One `[providers.<id>]` entry, its `type` key naming the variant.
///
/// The HTTP variants also take the [`HttpOverrides`] keys (`http_headers`,
/// `env_http_headers`, `query_params`) inline.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderConfig {
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
        extras: HttpOverrides,
    },

    /// Is any OpenAI-compatible HTTP server (Groq, Together, DeepSeek, LM Studio, …); it differs
    /// from `Openai` in requiring `base_url` and allowing no key.
    OpenaiCompat {
        #[serde(deserialize_with = "de_trimmed_string")]
        base_url: String,
        #[serde(default = "auth_none")]
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        /// Names who serves the API, in OpenTelemetry GenAI's words where it has one (`groq`,
        /// `mistral`, `deepseek`); the server behind the URL could be anyone's, so the block says.
        #[serde(deserialize_with = "de_trimmed_string")]
        gen_ai_provider_name: String,
        #[serde(flatten)]
        extras: HttpOverrides,
        #[serde(flatten)]
        capabilities: CapabilitiesOverrides,
    },

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
        extras: HttpOverrides,
    },

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
        extras: HttpOverrides,
    },

    Vllm {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        base_url: Option<String>,
        #[serde(default = "auth_none")]
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        #[serde(flatten)]
        extras: HttpOverrides,
        #[serde(flatten)]
        capabilities: CapabilitiesOverrides,
    },

    /// Is `mlx_lm.server` on Apple Silicon. Its wire format is OpenAI-compatible; the variant
    /// tells it apart in logs and capability defaults.
    Mlx {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        base_url: Option<String>,
        #[serde(default = "auth_none")]
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        #[serde(flatten)]
        extras: HttpOverrides,
        #[serde(flatten)]
        capabilities: CapabilitiesOverrides,
    },

    /// Is llama.cpp's `llama-server`, OpenAI-compatible like the other local backends.
    Llamacpp {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        base_url: Option<String>,
        #[serde(default = "auth_none")]
        auth: Auth,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        #[serde(flatten)]
        extras: HttpOverrides,
        #[serde(flatten)]
        capabilities: CapabilitiesOverrides,
    },

    /// Is the Claude Code CLI on the user's subscription.
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
        /// Sets `--tools`: `"disabled"` (the default) turns off the CLI's own agent loop without
        /// touching auth, `"default"` keeps it, and a list allows just those tools.
        #[serde(default)]
        tools: ClaudeCliToolsConfig,
        /// Sets `--bare`, which skips auto-memory and CLAUDE.md discovery but also turns off
        /// OAuth and keychain auth, requiring `ANTHROPIC_API_KEY`; a user of `claude login`
        /// breaks with it on.
        #[serde(default)]
        bare: bool,
        /// Sets `--effort`; `None` omits the flag.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<ClaudeCliEffortConfig>,
        /// Sets `--exclude-dynamic-system-prompt-sections`, on by default for prompt-cache reuse.
        #[serde(default = "default_true")]
        exclude_dynamic_sections: bool,
        /// Lists raw argv tokens appended to every call.
        #[serde(default)]
        extra_args: Vec<String>,
    },

    /// Is a long-lived `@anthropic-ai/claude-agent-sdk` Node child that serves requests as NDJSON
    /// over its stdin and stdout. The child holds the subscription login and the prompt cache;
    /// it lives as long as tars, so calls skip the cold start of a fresh `claude` per request.
    /// The child script is `tools/claude-daemon/server.mjs --stdio`.
    ClaudeSdk {
        /// Names the Node-compatible runtime that hosts the script (`node`, `bun`, a pinned path).
        #[serde(
            default = "default_node_executable",
            deserialize_with = "de_trimmed_string"
        )]
        executable: String,
        /// Points at `server.mjs`. When unset the backend tries `$TARS_CLAUDE_SDK_SCRIPT`, then
        /// `tools/claude-daemon/server.mjs` in the working directory or an ancestor, then
        /// `~/.tars/claude-daemon/server.mjs`.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "de_trimmed_opt_string"
        )]
        script_path: Option<String>,
        /// Bounds the wait for one reply, in seconds; the child itself stays up across calls.
        #[serde(default = "default_cli_timeout_secs")]
        timeout_secs: u64,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
    },

    /// Is the OpenAI Codex CLI on a ChatGPT subscription.
    CodexCli {
        #[serde(
            default = "default_codex_executable",
            deserialize_with = "de_trimmed_string"
        )]
        executable: String,
        /// Bounds one call, in seconds; it defaults to 600 rather than 300 because `codex exec`
        /// runs a full agent loop.
        #[serde(default = "default_codex_timeout_secs")]
        timeout_secs: u64,
        /// Confines codex's own tools (its shell, apply-patch), not tars's; it defaults to
        /// `read-only` so a call does not change files unexpectedly.
        #[serde(default)]
        sandbox: CodexSandboxConfig,
        /// Passes `--skip-git-repo-check`, on by default because tars often runs codex outside a
        /// git repository, which codex otherwise refuses.
        #[serde(default = "default_true")]
        skip_git_repo_check: bool,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
    },

    /// Is the opencode CLI, run as `opencode run --format json --model <provider/model> <prompt>`;
    /// `default_model` is in opencode's `provider/model` form (`anthropic/claude-sonnet-4-5`).
    Opencode {
        #[serde(
            default = "default_opencode_executable",
            deserialize_with = "de_trimmed_string"
        )]
        executable: String,
        #[serde(default = "default_cli_timeout_secs")]
        timeout_secs: u64,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
    },

    /// Is the Antigravity CLI, run as
    /// `agy -p "<prompt>" --model <model> --dangerously-skip-permissions --add-dir <cwd>`, whose
    /// stdout is the plain-text answer.
    Antigravity {
        #[serde(
            default = "default_antigravity_executable",
            deserialize_with = "de_trimmed_string"
        )]
        executable: String,
        #[serde(default = "default_cli_timeout_secs")]
        timeout_secs: u64,
        #[serde(deserialize_with = "de_trimmed_string")]
        default_model: String,
        /// Sets `agy --effort`, which `gemini-3.1-pro` requires.
        #[serde(default = "default_antigravity_effort")]
        effort: AntigravityEffortConfig,
    },

    /// Is an in-process provider that answers every call with `canned_response`.
    Mock {
        #[serde(default = "default_mock_response")]
        canned_response: String,
    },

    /// Replays recorded LLM responses, for tests. If `path` exists it is replayed, and a request
    /// the recording does not cover is an error, since it means a prompt changed. If it does not
    /// and `record_from` names another provider, that provider's responses are recorded and
    /// written to `path` when the run ends.
    Cassette {
        /// Points at a JSON object from request fingerprint to response text.
        #[serde(deserialize_with = "de_trimmed_string")]
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        record_from: Option<ProviderId>,
        /// Is the default model the registry records for this provider.
        #[serde(default = "default_cassette_model")]
        default_model: String,
    },
}

fn default_cassette_model() -> String {
    "cassette".into()
}

fn default_claude_executable() -> String {
    "claude".into()
}

fn default_codex_executable() -> String {
    "codex".into()
}

fn default_opencode_executable() -> String {
    "opencode".into()
}

fn default_antigravity_executable() -> String {
    "agy".into()
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

/// The TOML spelling of `tars_llm`'s codex `SandboxMode`; the registry converts it.
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSandboxConfig {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// The TOML spelling of `tars_llm::ClaudeToolAccess`, a keyword or an allow-list:
///
/// ```toml
/// tools = "disabled"
/// tools = "default"
/// tools = ["Read", "Bash"]
/// ```
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ClaudeCliToolsConfig {
    Named(ClaudeCliToolsKeyword),
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

/// The TOML spelling of `tars_llm::ClaudeCliEffort`.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeCliEffortConfig {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// The TOML spelling of `tars_llm::AntigravityEffort`; `agy` takes only `low` and `high`.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AntigravityEffortConfig {
    Low,
    High,
}

fn default_antigravity_effort() -> AntigravityEffortConfig {
    AntigravityEffortConfig::High
}

fn default_mock_response() -> String {
    "LGTM — no issues found.".into()
}

/// What the user declares of a local or compatible server in its provider block, for a model
/// the data file does not describe. A capability left out stays as the data file has it, which
/// for a local model is unknown: the request is sent and the server answers for itself. A
/// declared `false` makes tars refuse a request that needs it before sending. Every such server
/// speaks OpenAI's protocol, so the structured-output mechanisms it can be declared to have are
/// the ones that protocol sends.
///
/// ```toml
/// [providers.mlx_local]
/// type = "mlx"
/// default_model = "..."
/// max_context_tokens = 262144   # Qwen3's 256K window
/// max_output_tokens = 32768
/// supports_thinking = true
/// supports_vision = false
/// supports_structured_output = "json_object"   # or "strict_schema", "none"
/// supports_chat_template_thinking = true        # send chat_template_kwargs.enable_thinking
/// ```
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CapabilitiesOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_tool_use: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_structured_output: Option<OpenAiStructuredOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_chat_template_thinking: Option<bool>,
}

/// A structured-output mechanism a server reached over OpenAI's protocol can have: the protocol
/// has `response_format` and nothing that stands a forced tool call in for it.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiStructuredOutput {
    /// The server holds output to no schema.
    #[serde(rename = "none")]
    NoMechanism,
    /// `{"type":"json_object"}`: JSON of any shape.
    JsonObject,
    /// `{"type":"json_schema"}`: the schema is enforced while decoding.
    StrictSchema,
}

impl From<OpenAiStructuredOutput> for tars_types::StructuredOutputMode {
    fn from(mode: OpenAiStructuredOutput) -> Self {
        match mode {
            OpenAiStructuredOutput::NoMechanism => Self::None,
            OpenAiStructuredOutput::JsonObject => Self::JsonObjectMode,
            OpenAiStructuredOutput::StrictSchema => Self::StrictSchema,
        }
    }
}

impl CapabilitiesOverrides {
    /// Returns whether no override is set.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Returns `base` with each set override written in, replacing whatever the data file gave;
    /// an unset one leaves it alone. Parallel tool calls and tool-use emulation are made of tool
    /// calls, so `supports_tool_use = false` turns both off. A consistent `base` stays
    /// consistent: no declaration can contradict another or the data file.
    pub fn apply_to(&self, mut base: tars_types::ProviderProfile) -> tars_types::ProviderProfile {
        if let Some(n) = self.max_context_tokens {
            base.max_context_tokens = Some(n);
        }
        if let Some(n) = self.max_output_tokens {
            base.max_output_tokens = Some(n);
        }
        if let Some(b) = self.supports_tool_use {
            base.supports_tool_use = b.into();
            if !b {
                base.supports_parallel_tool_calls = false;
                if base.supports_structured_output
                    == Some(tars_types::StructuredOutputMode::ToolUseEmulation)
                {
                    base.supports_structured_output = Some(tars_types::StructuredOutputMode::None);
                }
            }
        }
        if let Some(b) = self.supports_thinking {
            base.supports_thinking = b.into();
        }
        if let Some(b) = self.supports_vision {
            base.supports_vision = b.into();
        }
        if let Some(mode) = self.supports_structured_output {
            base.supports_structured_output = Some(mode.into());
        }
        if let Some(b) = self.supports_chat_template_thinking {
            base.supports_chat_template_thinking = b;
        }
        base
    }
}

fn auth_none() -> Auth {
    Auth::None
}

/// Names which kind of provider a `[providers.<id>]` block declares: its `type` key. The kind
/// alone fixes how tars reaches the provider, whether the operator runs its server, and which
/// data-file block describes its models.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Openai,
    OpenaiCompat,
    Anthropic,
    Gemini,
    Vllm,
    Mlx,
    Llamacpp,
    ClaudeCli,
    ClaudeSdk,
    CodexCli,
    Opencode,
    Antigravity,
    Mock,
    Cassette,
}

impl ProviderKind {
    /// Returns the `type` word that declares this kind in TOML.
    pub fn as_str(self) -> &'static str {
        use ProviderKind::*;
        match self {
            Openai => "openai",
            OpenaiCompat => "openai_compat",
            Anthropic => "anthropic",
            Gemini => "gemini",
            Vllm => "vllm",
            Mlx => "mlx",
            Llamacpp => "llamacpp",
            ClaudeCli => "claude_cli",
            ClaudeSdk => "claude_sdk",
            CodexCli => "codex_cli",
            Opencode => "opencode",
            Antigravity => "antigravity",
            Mock => "mock",
            Cassette => "cassette",
        }
    }

    /// Returns the `data/provider.toml` block that describes this kind's models, or `None` for
    /// `openai_compat`, whose block is named after the provider id (`deepseek`).
    pub fn catalog_block(self) -> Option<&'static str> {
        match self {
            ProviderKind::OpenaiCompat => None,
            kind => Some(kind.as_str()),
        }
    }

    /// Returns how tars reaches a provider of this kind. `data/provider.toml` also states an
    /// `interface` per block; a test in `builtin.rs` checks that the built-ins agree with it.
    pub fn interface(self) -> tars_types::InterfaceKind {
        use ProviderKind::*;
        use tars_types::InterfaceKind as Ik;
        match self {
            ClaudeCli | CodexCli | Opencode | Antigravity => Ik::Cli,
            Openai | OpenaiCompat | Anthropic | Gemini | Vllm | Mlx | Llamacpp => Ik::Http,
            ClaudeSdk => Ik::Api,
            Mock | Cassette => Ik::Mock,
        }
    }

    /// Returns whether the operator runs this kind's server on their own machine (vllm, mlx,
    /// llama.cpp), so that a refused connection means the server is not started.
    pub fn self_hosted(self) -> bool {
        use ProviderKind::*;
        match self {
            Vllm | Mlx | Llamacpp => true,
            Openai | OpenaiCompat | Anthropic | Gemini | ClaudeCli | ClaudeSdk | CodexCli
            | Opencode | Antigravity | Mock | Cassette => false,
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ProviderConfig {
    /// Returns the kind this block declares.
    pub fn kind(&self) -> ProviderKind {
        use ProviderConfig::*;
        match self {
            Openai { .. } => ProviderKind::Openai,
            OpenaiCompat { .. } => ProviderKind::OpenaiCompat,
            Anthropic { .. } => ProviderKind::Anthropic,
            Gemini { .. } => ProviderKind::Gemini,
            Vllm { .. } => ProviderKind::Vllm,
            Mlx { .. } => ProviderKind::Mlx,
            Llamacpp { .. } => ProviderKind::Llamacpp,
            ClaudeCli { .. } => ProviderKind::ClaudeCli,
            ClaudeSdk { .. } => ProviderKind::ClaudeSdk,
            CodexCli { .. } => ProviderKind::CodexCli,
            Opencode { .. } => ProviderKind::Opencode,
            Antigravity { .. } => ProviderKind::Antigravity,
            Mock { .. } => ProviderKind::Mock,
            Cassette { .. } => ProviderKind::Cassette,
        }
    }

    /// Returns the `data/provider.toml` block that describes the models of the provider declared
    /// as `id`: the kind's block for a typed provider (`gemini_flash` of type `gemini` reads
    /// `gemini`), and `id` itself for `openai_compat` (`deepseek`). The block need not exist; a
    /// missing one has no rows and no series.
    pub fn catalog_name<'a>(&self, id: &'a str) -> &'a str {
        self.kind().catalog_block().unwrap_or(id)
    }

    /// Returns the catalog block a live model list from the provider declared as `id` stands
    /// for. A hosted vendor's list stands for the vendor's block only when it came from the
    /// vendor's own endpoint; one from a custom `base_url` (a gateway, a proxy) stands for `id`
    /// alone, so it never decides the vendor's `@latest` or limits.
    pub fn listing_catalog<'a>(&self, id: &'a str) -> &'a str {
        use ProviderConfig::*;
        match self {
            Openai {
                base_url: Some(_), ..
            }
            | Anthropic {
                base_url: Some(_), ..
            }
            | Gemini {
                base_url: Some(_), ..
            } => id,
            Openai { base_url: None, .. }
            | Anthropic { base_url: None, .. }
            | Gemini { base_url: None, .. }
            | OpenaiCompat { .. }
            | Vllm { .. }
            | Mlx { .. }
            | Llamacpp { .. }
            | ClaudeCli { .. }
            | ClaudeSdk { .. }
            | CodexCli { .. }
            | Opencode { .. }
            | Antigravity { .. }
            | Mock { .. }
            | Cassette { .. } => self.catalog_name(id),
        }
    }

    /// Returns where the credential of the provider's calls comes from.
    pub fn auth(&self) -> AuthSetting<'_> {
        use ProviderConfig::*;
        match self {
            Openai { auth, .. }
            | OpenaiCompat { auth, .. }
            | Anthropic { auth, .. }
            | Gemini { auth, .. }
            | Vllm { auth, .. }
            | Mlx { auth, .. }
            | Llamacpp { auth, .. } => AuthSetting::Configured(auth),
            ClaudeCli { .. }
            | ClaudeSdk { .. }
            | CodexCli { .. }
            | Opencode { .. }
            | Antigravity { .. } => AuthSetting::OwnLogin,
            Mock { .. } | Cassette { .. } => AuthSetting::NoCall,
        }
    }

    /// Returns the configured default model spec (`"mock-model"` for Mock).
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
            | CodexCli { default_model, .. }
            | Opencode { default_model, .. }
            | Antigravity { default_model, .. }
            | Cassette { default_model, .. } => default_model,
            Mock { .. } => "mock-model",
        }
    }

    /// Returns this config with `timeout_secs` replaced, for the CLI and SDK backends that carry
    /// one; the others come back unchanged. A provider fixes its timeout when built, so a caller
    /// that needs a longer one for a single call builds a fresh provider from the result.
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        use ProviderConfig::*;
        match &mut self {
            ClaudeCli { timeout_secs, .. }
            | ClaudeSdk { timeout_secs, .. }
            | CodexCli { timeout_secs, .. }
            | Opencode { timeout_secs, .. }
            | Antigravity { timeout_secs, .. } => *timeout_secs = secs,
            // Listed by name, not `_`, so a new backend with a timeout does not compile until it
            // is handled here. The HTTP backends time out in their HTTP layer; a
            // cassette's timeout is the wrapped provider's.
            Openai { .. }
            | OpenaiCompat { .. }
            | Anthropic { .. }
            | Gemini { .. }
            | Vllm { .. }
            | Mlx { .. }
            | Llamacpp { .. }
            | Mock { .. }
            | Cassette { .. } => {}
        }
        self
    }

    /// Reports into `sink` what is wrong with this entry by itself: empty required strings, a
    /// missing key where the provider needs one, zero token caps, an out-of-range timeout.
    pub fn validate_self(&self, id: &ProviderId, sink: &mut Vec<ValidationError>) {
        let key = |k: &str| format!("providers.{id}.{k}");

        let check_default_model = |dm: &str, sink: &mut Vec<ValidationError>| {
            if let Some(problem) = model_spec_problem(dm, ValidationProblem::Empty) {
                sink.push(ValidationError::new(key("default_model"), problem));
            }
        };
        let check_opt_base_url = |opt: &Option<String>, sink: &mut Vec<ValidationError>| {
            if matches!(opt, Some(s) if s.trim().is_empty()) {
                sink.push(ValidationError::new(
                    key("base_url"),
                    ValidationProblem::EmptyWhenSet,
                ));
            }
        };
        // `apply_to` would write a zero cap into the profile.
        let check_capabilities = |caps: &CapabilitiesOverrides, sink: &mut Vec<ValidationError>| {
            if matches!(caps.max_context_tokens, Some(0)) {
                sink.push(ValidationError::new(
                    key("max_context_tokens"),
                    ValidationProblem::ZeroWhenSet,
                ));
            }
            if matches!(caps.max_output_tokens, Some(0)) {
                sink.push(ValidationError::new(
                    key("max_output_tokens"),
                    ValidationProblem::ZeroWhenSet,
                ));
            }
        };
        let check_timeout = |secs: u64, sink: &mut Vec<ValidationError>| {
            if secs == 0 {
                sink.push(ValidationError::new(
                    key("timeout_secs"),
                    ValidationProblem::Zero,
                ));
            } else if secs > MAX_CLI_TIMEOUT_SECS {
                sink.push(ValidationError::new(
                    key("timeout_secs"),
                    ValidationProblem::TimeoutTooLong {
                        max: MAX_CLI_TIMEOUT_SECS,
                    },
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
                if !sends_a_key(auth) {
                    sink.push(ValidationError::new(
                        key("auth"),
                        ValidationProblem::KeyRequired {
                            kind: ProviderKind::Openai,
                        },
                    ));
                }
                check_default_model(default_model, sink);
            }
            ProviderConfig::OpenaiCompat {
                base_url,
                default_model,
                gen_ai_provider_name,
                capabilities,
                ..
            } => {
                if base_url.trim().is_empty() {
                    sink.push(ValidationError::new(
                        key("base_url"),
                        ValidationProblem::CompatBaseUrlRequired,
                    ));
                }
                if gen_ai_provider_name.is_empty() {
                    sink.push(ValidationError::new(
                        key("gen_ai_provider_name"),
                        ValidationProblem::ServerNameRequired,
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
                if matches!(api_version, Some(s) if s.trim().is_empty()) {
                    sink.push(ValidationError::new(
                        key("api_version"),
                        ValidationProblem::EmptyWhenSet,
                    ));
                }
                if !sends_a_key(auth) {
                    sink.push(ValidationError::new(
                        key("auth"),
                        ValidationProblem::KeyRequired {
                            kind: ProviderKind::Anthropic,
                        },
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
                if !sends_a_key(auth) {
                    sink.push(ValidationError::new(
                        key("auth"),
                        ValidationProblem::KeyRequired {
                            kind: ProviderKind::Gemini,
                        },
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
            | ProviderConfig::Opencode {
                executable,
                timeout_secs,
                default_model,
            }
            | ProviderConfig::Antigravity {
                executable,
                timeout_secs,
                default_model,
                ..
            } => {
                if executable.trim().is_empty() {
                    sink.push(ValidationError::new(
                        key("executable"),
                        ValidationProblem::Empty,
                    ));
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
                    sink.push(ValidationError::new(
                        key("executable"),
                        ValidationProblem::Empty,
                    ));
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
                    sink.push(ValidationError::new(
                        key("executable"),
                        ValidationProblem::Empty,
                    ));
                }
                // An empty path would stop the backend's search for the script.
                if let Some(sp) = script_path {
                    if sp.trim().is_empty() {
                        sink.push(ValidationError::new(
                            key("script_path"),
                            ValidationProblem::EmptyScriptPath,
                        ));
                    }
                }
                check_timeout(*timeout_secs, sink);
                check_default_model(default_model, sink);
            }
            ProviderConfig::Mock { .. } => {}
            ProviderConfig::Cassette {
                path,
                default_model,
                ..
            } => {
                if path.trim().is_empty() {
                    sink.push(ValidationError::new(key("path"), ValidationProblem::Empty));
                }
                check_default_model(default_model, sink);
            }
        }
    }
}

/// Returns whether `auth` puts a key on an HTTP request: `None` sends nothing, and so does
/// `Delegate`, which only a spawned CLI that holds its own login can use.
fn sends_a_key(auth: &Auth) -> bool {
    match auth {
        Auth::Secret { .. } => true,
        Auth::None | Auth::Delegate => false,
    }
}

/// Returns what is wrong with `spec` as a model spec: `blank` when it names no model, the parse
/// error when it does not parse, `None` when it is a spec.
pub(crate) fn model_spec_problem(
    spec: &str,
    blank: ValidationProblem,
) -> Option<ValidationProblem> {
    match crate::model_spec::ModelSpec::parse(spec) {
        Ok(_) => None,
        Err(crate::model_spec::ParseModelSpecError::Blank { .. }) => Some(blank),
        Err(e) => Some(ValidationProblem::UnparsableModelSpec(e)),
    }
}

/// Caps a CLI timeout at 24 hours, in seconds: a per-call timeout longer than a day is a typo.
const MAX_CLI_TIMEOUT_SECS: u64 = 86_400;

impl ProvidersConfig {
    /// Runs [`ProviderConfig::validate_self`] over every entry and checks that each cassette's
    /// `record_from` names a provider it can record. An empty table is not an error: the loader
    /// always merges the built-ins in.
    pub fn validate(&self, sink: &mut Vec<ValidationError>) {
        for (id, cfg) in &self.providers {
            cfg.validate_self(id, sink);
            if let ProviderConfig::Cassette {
                record_from: Some(source),
                ..
            } = cfg
            {
                if let Some(problem) = record_source_problem(source, self.providers.get(source)) {
                    sink.push(ValidationError::new(
                        format!("providers.{id}.record_from"),
                        problem,
                    ));
                }
            }
        }
    }
}

/// Returns why a cassette cannot record from `source`, which resolves to `entry`: it is not
/// declared, or it is a cassette (itself included), which makes no call to record.
fn record_source_problem(
    source: &ProviderId,
    entry: Option<&ProviderConfig>,
) -> Option<ValidationProblem> {
    match entry {
        None => Some(ValidationProblem::UnknownProvider {
            id: source.to_string(),
        }),
        Some(ProviderConfig::Cassette { .. }) => Some(ValidationProblem::RecordsFromCassette {
            id: source.to_string(),
        }),
        Some(_) => None,
    }
}

/// Where the credential of a provider's calls comes from.
#[derive(Clone, Copy, Debug)]
pub enum AuthSetting<'a> {
    /// The provider block's `auth` field.
    Configured(&'a Auth),
    /// The block has no `auth` field: the CLI or the SDK daemon logs in on its own.
    OwnLogin,
    /// The provider places no call (mock, cassette), so nothing authenticates.
    NoCall,
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
        let back = toml::to_string(&cfg).unwrap();
        assert!(
            !back.contains("max_context_tokens"),
            "empty overrides must not emit the field; got: {back}",
        );
    }

    #[test]
    fn capabilities_overrides_apply_to_replaces_only_set_fields() {
        let mut base =
            tars_types::ProviderProfile::text_only_baseline(tars_types::Pricing::default());
        base.max_context_tokens = Some(4096);
        base.max_output_tokens = Some(1024);

        let overrides = CapabilitiesOverrides {
            max_context_tokens: Some(262144),
            ..CapabilitiesOverrides::default()
        };
        let p = overrides.apply_to(base);
        assert_eq!(p.max_context_tokens, Some(262144));
        assert_eq!(p.max_output_tokens, Some(1024));
    }

    #[test]
    fn a_declared_capability_replaces_what_the_data_file_does_not_know() {
        let cfg: ProviderConfig = toml::from_str(
            r#"
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://localhost:1234/v1"
            default_model = "m"
            supports_tool_use = true
            supports_vision = false
        "#,
        )
        .unwrap();
        let ProviderConfig::OpenaiCompat { capabilities, .. } = cfg else {
            panic!("wrong variant");
        };
        let p = capabilities.apply_to(tars_types::ProviderProfile::undescribed_http());
        assert_eq!(p.supports_tool_use, tars_types::Support::Supported);
        assert_eq!(p.supports_vision, tars_types::Support::Unsupported);
        assert_eq!(p.supports_thinking, tars_types::Support::Unknown);
    }

    #[test]
    fn tool_use_declared_off_turns_off_parallel_calls_and_tool_use_emulation() {
        let mut parallel = tars_types::ProviderProfile::undescribed_http();
        parallel.supports_tool_use = tars_types::Support::Supported;
        parallel.supports_parallel_tool_calls = true;
        parallel.supports_structured_output =
            Some(tars_types::StructuredOutputMode::ToolUseEmulation);
        let no_tools = CapabilitiesOverrides {
            supports_tool_use: Some(false),
            ..CapabilitiesOverrides::default()
        };
        let off = no_tools.apply_to(parallel.clone());
        assert!(!off.supports_parallel_tool_calls);
        assert_eq!(
            off.supports_structured_output,
            Some(tars_types::StructuredOutputMode::None)
        );
        assert!(off.validate().is_ok());

        let with_tools = CapabilitiesOverrides {
            supports_tool_use: Some(true),
            ..CapabilitiesOverrides::default()
        };
        let on = with_tools.apply_to(parallel);
        assert!(on.supports_parallel_tool_calls);
        assert_eq!(
            on.supports_structured_output,
            Some(tars_types::StructuredOutputMode::ToolUseEmulation)
        );
    }

    fn chat_template_thinking(default: bool, declared: Option<bool>) -> bool {
        let mut base = tars_types::ProviderProfile::undescribed_http();
        base.supports_chat_template_thinking = default;
        let overrides = CapabilitiesOverrides {
            supports_chat_template_thinking: declared,
            ..CapabilitiesOverrides::default()
        };
        overrides.apply_to(base).supports_chat_template_thinking
    }

    #[test]
    fn chat_template_thinking_left_undeclared_keeps_the_profiles_default() {
        assert!(chat_template_thinking(true, None));
        assert!(!chat_template_thinking(false, None));
    }

    #[test]
    fn chat_template_thinking_declared_true_holds_over_a_default_of_false() {
        assert!(chat_template_thinking(false, Some(true)));
        assert!(chat_template_thinking(true, Some(true)));
    }

    #[test]
    fn chat_template_thinking_declared_false_holds_over_a_default_of_true() {
        assert!(!chat_template_thinking(true, Some(false)));
        assert!(!chat_template_thinking(false, Some(false)));
    }

    #[test]
    fn tool_use_emulation_cannot_be_declared_for_a_server_on_openais_protocol() {
        for refused in ["tool_use_emulation", "json_object_mode"] {
            let cfg = format!(
                r#"
                type = "openai_compat"
                gen_ai_provider_name = "local"
                base_url = "http://localhost:1234/v1"
                default_model = "m"
                supports_structured_output = "{refused}"
            "#
            );
            assert!(toml::from_str::<ProviderConfig>(&cfg).is_err(), "{refused}");
        }
        for (accepted, mode) in [
            ("none", tars_types::StructuredOutputMode::None),
            (
                "json_object",
                tars_types::StructuredOutputMode::JsonObjectMode,
            ),
            (
                "strict_schema",
                tars_types::StructuredOutputMode::StrictSchema,
            ),
        ] {
            let cfg = format!(
                r#"
                type = "openai_compat"
                gen_ai_provider_name = "local"
                base_url = "http://localhost:1234/v1"
                default_model = "m"
                supports_structured_output = "{accepted}"
            "#
            );
            let ProviderConfig::OpenaiCompat { capabilities, .. } =
                toml::from_str::<ProviderConfig>(&cfg).unwrap()
            else {
                panic!("wrong variant");
            };
            let p = capabilities.apply_to(tars_types::ProviderProfile::undescribed_http());
            assert_eq!(p.supports_structured_output, Some(mode), "{accepted}");
        }
    }

    #[test]
    fn a_declared_structured_output_replaces_the_unknown_one() {
        let cfg: ProviderConfig = toml::from_str(
            r#"
            type = "openai_compat"
            gen_ai_provider_name = "local"
            base_url = "http://localhost:1234/v1"
            default_model = "m"
            supports_structured_output = "json_object"
        "#,
        )
        .unwrap();
        let ProviderConfig::OpenaiCompat { capabilities, .. } = cfg else {
            panic!("wrong variant");
        };
        let p = capabilities.apply_to(tars_types::ProviderProfile::undescribed_http());
        assert_eq!(
            p.supports_structured_output,
            Some(tars_types::StructuredOutputMode::JsonObjectMode)
        );
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
                assert_eq!(timeout_secs, 600);
                assert_eq!(sandbox, CodexSandboxConfig::ReadOnly);
                assert!(skip_git_repo_check);
                assert_eq!(default_model, "gpt-5");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn with_timeout_secs_patches_cli_variants_and_noops_http() {
        let cli: ProviderConfig = toml::from_str(
            "type = \"claude_cli\"\ndefault_model = \"claude-sonnet-4-5\"\ntimeout_secs = 300\n",
        )
        .unwrap();
        match cli.with_timeout_secs(1800) {
            ProviderConfig::ClaudeCli { timeout_secs, .. } => assert_eq!(timeout_secs, 1800),
            other => panic!("wrong variant: {other:?}"),
        }

        let none: ProviderConfig =
            toml::from_str("type = \"mock\"\ncanned_response = \"hi\"\n").unwrap();
        assert!(
            matches!(none.with_timeout_secs(1800), ProviderConfig::Mock { .. }),
            "mock has no timeout_secs; with_timeout_secs must be a no-op that preserves the variant"
        );
    }

    #[test]
    fn opencode_defaults_executable_and_timeout() {
        let toml_str = r#"
            type = "opencode"
            default_model = "anthropic/claude-sonnet-4-5"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match &cfg {
            ProviderConfig::Opencode {
                executable,
                timeout_secs,
                default_model,
            } => {
                assert_eq!(executable, "opencode");
                assert_eq!(*timeout_secs, 300);
                assert_eq!(default_model, "anthropic/claude-sonnet-4-5");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        assert_eq!(cfg.default_model(), "anthropic/claude-sonnet-4-5");
    }

    #[test]
    fn antigravity_defaults_executable_and_timeout() {
        let toml_str = r#"
            type = "antigravity"
            default_model = "gemini-2.5-pro"
        "#;
        let cfg: ProviderConfig = toml::from_str(toml_str).unwrap();
        match cfg {
            ProviderConfig::Antigravity {
                executable,
                timeout_secs,
                default_model,
                effort,
            } => {
                assert_eq!(executable, "agy");
                assert_eq!(timeout_secs, 300);
                assert_eq!(default_model, "gemini-2.5-pro");
                assert_eq!(effort, AntigravityEffortConfig::High);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn opencode_and_antigravity_validate_empty_default_model() {
        for cfg in [
            ProviderConfig::Opencode {
                executable: "opencode".into(),
                timeout_secs: 300,
                default_model: String::new(),
            },
            ProviderConfig::Antigravity {
                executable: "agy".into(),
                timeout_secs: 300,
                default_model: String::new(),
                effort: AntigravityEffortConfig::High,
            },
        ] {
            let id = ProviderId::new("d");
            let mut errs = Vec::new();
            cfg.validate_self(&id, &mut errs);
            assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
            assert_eq!(errs[0].key, "providers.d.default_model");
        }
    }

    #[test]
    fn codex_cli_sandbox_kebab_case_round_trips() {
        // A renamed spelling must fail here, not silently change codex's sandbox.
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
    fn a_hosted_provider_with_delegate_auth_is_refused_like_none() {
        for (ty, kind) in [
            ("openai", ProviderKind::Openai),
            ("anthropic", ProviderKind::Anthropic),
            ("gemini", ProviderKind::Gemini),
        ] {
            let toml_str = format!(
                "type = \"{ty}\"\nauth = {{ kind = \"delegate\" }}\ndefault_model = \"m\"\n"
            );
            let cfg: ProviderConfig = toml::from_str(&toml_str).unwrap();
            let mut errs = Vec::new();
            cfg.validate_self(&ProviderId::new("p"), &mut errs);
            assert_eq!(
                errs,
                vec![ValidationError::new(
                    "providers.p.auth",
                    ValidationProblem::KeyRequired { kind }
                )],
                "{ty}"
            );
        }
    }

    #[test]
    fn a_cassette_records_only_from_a_declared_provider_that_calls() {
        let cassette = ProviderConfig::Cassette {
            path: "tape.json".into(),
            record_from: None,
            default_model: "m".into(),
        };
        let caller = ProviderConfig::Mock {
            canned_response: "ok".into(),
        };
        let source = ProviderId::new("src");
        assert_eq!(
            record_source_problem(&source, None),
            Some(ValidationProblem::UnknownProvider { id: "src".into() })
        );
        assert_eq!(
            record_source_problem(&source, Some(&cassette)),
            Some(ValidationProblem::RecordsFromCassette { id: "src".into() })
        );
        assert_eq!(record_source_problem(&source, Some(&caller)), None);
    }

    #[test]
    fn validate_flags_anthropic_without_auth() {
        let cfg = ProviderConfig::Anthropic {
            base_url: None,
            api_version: None,
            auth: Auth::None,
            default_model: "claude-opus-4-7".into(),
            extras: HttpOverrides::default(),
        };
        let id = ProviderId::new("ant");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
        assert_eq!(errs[0].key, "providers.ant.auth");
        assert!(
            errs[0].problem.to_string().contains("api key"),
            "message should mention api key, got: {}",
            errs[0].problem
        );
    }

    #[test]
    fn validate_flags_gemini_without_auth() {
        let cfg = ProviderConfig::Gemini {
            base_url: None,
            auth: Auth::None,
            default_model: "gemini-2.5-pro".into(),
            extras: HttpOverrides::default(),
        };
        let id = ProviderId::new("gem");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
        assert_eq!(errs[0].key, "providers.gem.auth");
        assert!(
            errs[0].problem.to_string().contains("api key"),
            "message should mention api key, got: {}",
            errs[0].problem
        );
    }

    #[test]
    fn validate_flags_openai_without_auth() {
        let cfg = ProviderConfig::Openai {
            base_url: None,
            auth: Auth::None,
            default_model: "gpt-4o".into(),
            extras: HttpOverrides::default(),
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
            gen_ai_provider_name: "local".into(),
            extras: HttpOverrides::default(),
            capabilities: Default::default(),
        };
        let id = ProviderId::new("compat");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
        assert_eq!(errs[0].key, "providers.compat.base_url");
    }

    #[test]
    fn validate_flags_openai_compat_that_names_nobody_serving_it() {
        let cfg = ProviderConfig::OpenaiCompat {
            base_url: "http://localhost:8000/v1".into(),
            auth: Auth::None,
            default_model: "x".into(),
            gen_ai_provider_name: String::new(),
            extras: HttpOverrides::default(),
            capabilities: Default::default(),
        };
        let id = ProviderId::new("compat");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
        assert_eq!(errs[0].key, "providers.compat.gen_ai_provider_name");
    }

    #[test]
    fn an_openai_compat_block_without_gen_ai_provider_name_does_not_load() {
        let raw = r#"
            type = "openai_compat"
            base_url = "http://localhost:8000/v1"
            default_model = "x"
        "#;
        let err = toml::from_str::<ProviderConfig>(raw).unwrap_err();
        assert!(err.to_string().contains("gen_ai_provider_name"), "{err}");
    }

    #[test]
    fn validate_flags_http_provider_with_empty_default_model() {
        let cfg = ProviderConfig::Anthropic {
            base_url: None,
            api_version: None,
            auth: Auth::env("ANTHROPIC_API_KEY"),
            default_model: String::new(),
            extras: HttpOverrides::default(),
        };
        let id = ProviderId::new("ant");
        let mut errs = Vec::new();
        cfg.validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].key, "providers.ant.default_model");
    }

    #[test]
    fn a_cassette_recording_from_an_empty_provider_id_does_not_load() {
        let toml_str = "type = \"cassette\"\npath = \"tape.json\"\nrecord_from = \"\"\n";
        let err = toml::from_str::<ProviderConfig>(toml_str).unwrap_err();
        assert!(
            err.to_string().contains("ProviderId cannot be empty"),
            "{err}"
        );
    }

    #[test]
    fn a_cassette_with_a_blank_default_model_is_flagged_and_one_with_a_model_is_not() {
        let cassette = |default_model: &str| ProviderConfig::Cassette {
            path: "tape.json".into(),
            record_from: None,
            default_model: default_model.into(),
        };
        let id = ProviderId::new("tape");
        let mut errs = Vec::new();
        cassette(" ").validate_self(&id, &mut errs);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].key, "providers.tape.default_model");
        let mut errs = Vec::new();
        cassette("m").validate_self(&id, &mut errs);
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn validate_flags_optional_base_url_set_to_empty_string() {
        let cfg = ProviderConfig::Vllm {
            base_url: Some(String::new()),
            auth: Auth::None,
            default_model: "model".into(),
            extras: HttpOverrides::default(),
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
        let id = ProviderId::new("good");
        let mut errs = Vec::new();

        ProviderConfig::Anthropic {
            base_url: None,
            api_version: None,
            auth: Auth::env("ANTHROPIC_API_KEY"),
            default_model: "claude-opus-4-7".into(),
            extras: HttpOverrides::default(),
        }
        .validate_self(&id, &mut errs);

        ProviderConfig::Openai {
            base_url: None,
            auth: Auth::env("OPENAI_API_KEY"),
            default_model: "gpt-4o".into(),
            extras: HttpOverrides::default(),
        }
        .validate_self(&id, &mut errs);

        ProviderConfig::OpenaiCompat {
            base_url: "http://localhost:8000/v1".into(),
            auth: Auth::None,
            default_model: "Qwen/Qwen2.5-Coder-32B-Instruct".into(),
            gen_ai_provider_name: "local".into(),
            extras: HttpOverrides::default(),
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
            gen_ai_provider_name = "local"
            base_url = "http://localhost:8000/v1"
            default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"

            [claude_cli]
            type = "claude_cli"
            default_model = "claude-opus-4-7"
        "#;
        let cfg: ProvidersConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.len(), 3);

        // Field-level assertions catch fields swapped between providers.
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

    #[test]
    fn provider_type_string_matches_serde_tag() {
        let gemini = crate::builtin::default_gemini();
        assert_eq!(gemini.kind().as_str(), "gemini");
        let mut every: Vec<ProviderConfig> =
            crate::built_in_provider_defaults().into_values().collect();
        every.push(toml::from_str("type = \"mock\"").unwrap());
        for cfg in every {
            let tagged = serde_json::to_value(&cfg).unwrap();
            assert_eq!(tagged["type"], cfg.kind().as_str(), "{cfg:?}");
        }
    }

    #[test]
    fn a_gemini_behind_a_custom_base_url_lists_for_its_own_id() {
        let official = crate::builtin::default_gemini();
        assert_eq!(official.listing_catalog("gemini_flash"), "gemini");
        let gateway: ProviderConfig = toml::from_str(
            r#"
            type = "gemini"
            base_url = "https://my-gateway"
            auth = { kind = "none" }
            default_model = "flash@latest"
            "#,
        )
        .unwrap();
        assert_eq!(gateway.listing_catalog("gw"), "gw");
        assert_eq!(gateway.catalog_name("gw"), "gemini");
    }
}
