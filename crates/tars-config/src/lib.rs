//! Parses and validates the tars config: `<home>/config.toml` becomes one typed [`Config`],
//! installed once per process, with the built-in provider defaults and the model knowledge base
//! merged under it.
//!
//! It knows each provider's config schema and nothing about building a provider: turning a
//! [`ProviderConfig`] into one is `tars-llm`'s `ProviderRegistry::from_config`.

#![deny(clippy::print_stdout, clippy::print_stderr)]

pub mod builtin;
pub mod config;
pub mod error;
pub mod global;
pub mod model_catalog;
pub mod model_kb;
pub mod model_library;
pub mod model_spec;
pub mod paths;
pub mod providers;
pub mod roles;
pub mod sandbox;

pub use builtin::{
    built_in_provider_defaults, built_in_provider_ids, default_anthropic, default_claude_cli,
    default_gemini, default_openai, default_vllm, merge_builtin_with_user,
};
pub use config::Config;
pub use error::ConfigError;
pub use global::{LoadedHome, init_tars, install, load_tars};
pub use global::{
    ResolveStoreDirError, durable_store_path, get_bodydb_path, get_eventdb_path,
    get_pipelinedb_path,
};
pub use model_catalog::{
    CatalogError, ModelCatalog, OutputLimitRule, capabilities_for, catalog, install_catalog,
    load_catalog,
};
pub use model_kb::{
    BillingModel, KbModality, MODEL_KB, ModelEntry, ModelKb, ModelStatus, ModelTier,
    ProviderCapabilities, ProviderDef, ProviderModels, Thinking, ThinkingParam, Vendor, VendorDef,
};
pub use model_spec::{
    ModelSpec, ParseModelSpecError, ResolveModelSpecError, ResolvedModel, SeriesPattern,
};
pub use paths::{default_config_path, resolve_home};
pub use providers::{
    AntigravityEffortConfig, AuthSetting, CapabilitiesOverrides, ClaudeCliEffortConfig,
    ClaudeCliToolsConfig, ClaudeCliToolsKeyword, CodexSandboxConfig, OpenAiStructuredOutput,
    ProviderConfig, ProviderKind, ProvidersConfig,
};
pub use roles::RoleConfig;
pub use sandbox::{SandboxConfig, SandboxModeConfig, resolve_policy};
