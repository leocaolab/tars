//! tars-config — declarative configuration for TARS Runtime.
//!
//! Per Doc 06 (process isolation) the shape is a global immutable Config
//! loaded once from `~/.tars` (one tenant per process) plus a small
//! per-workspace `[roles]` overlay — NOT the old shared-process 5-layer
//! stack with hot reload / in-process tenant overrides (that is the
//! DEPRECATED appendix of Doc 06). This v0.1 ships only the pieces needed
//! to **declaratively configure providers** so the existing
//! `tars-provider` builders no longer need to be hand-wired:
//!
//! - [`Config`] — top-level container (only `providers` populated for now)
//! - [`ProvidersConfig`] / [`ProviderConfig`] — provider declarations
//!   covering every concrete backend in `tars-provider`
//! - [`ConfigManager`] — load + validate from a TOML file
//! - [`ConfigError`] — typed errors for all loader / validator paths
//!
//! Everything else (tenants, pipeline order, lock constraints, hot
//! reload, secret manager backends) lands in subsequent iterations.
//!
//! The crate intentionally has **no provider knowledge beyond schema** —
//! it doesn't know how to instantiate a [`ProviderConfig::OpenAi`]
//! into an `OpenAiProvider`. That happens in `tars-provider`'s
//! `ProviderRegistry::from_config()` factory, which depends on us.

pub mod builtin;
pub mod error;
pub mod global;
pub mod manager;
pub mod model_kb;
pub mod paths;
pub mod providers;
pub mod roles;
pub mod routing;
pub mod sandbox;
pub mod web_search;

pub use builtin::{
    built_in_provider_defaults, default_anthropic, default_claude_cli, default_gemini,
    default_openai, default_vllm, merge_builtin_with_user,
};
pub use error::ConfigError;
pub use global::resolve_home;
pub use global::{
    get_boarddb_path, get_bodydb_path, get_cachedb_path, get_durabledb_path, get_eventdb_path,
    get_pipelinedb_path,
};
pub use manager::{Config, ConfigManager};
pub use model_kb::{
    BillingModel, KbModality, MODEL_KB, ModelEntry, ModelKb, ModelStatus, ModelTier,
    PromptCacheSpec, ProviderCapabilities, ProviderDef, ProviderModels, Thinking, ThinkingParam,
    capabilities_for,
};
pub use paths::default_config_path;
pub use providers::{
    AntigravityEffortConfig, CapabilitiesOverrides, ClaudeCliEffortConfig, ClaudeCliToolsConfig,
    ClaudeCliToolsKeyword, CodexSandboxConfig, ProviderConfig, ProvidersConfig,
};
pub use roles::RoleConfig;
pub use routing::RoutingConfig;
pub use sandbox::{SandboxConfig, SandboxModeConfig, resolve_policy};
// `[web_search]` schema is owned by sisurf; we re-export it so consumers wire
// the resolved backend without a direct sisurf-core dependency of their own.
pub use sisurf_core::SearchConfig;
pub use web_search::{BRAVE_API_KEY_ENV, GOOGLE_CSE_API_KEY_ENV, inject_search_keys};
