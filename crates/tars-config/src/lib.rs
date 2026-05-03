//! tars-config — declarative configuration for TARS Runtime.
//!
//! Per Doc 06 the eventual shape is a 5-layer override stack
//! (Compiled → Built-in → System → User → Tenant → Per-Request) with
//! lock constraints, hot reload, and tenant-level overrides. This v0.1
//! ships only the pieces needed to **declaratively configure
//! providers** so the existing `tars-provider` builders no longer need
//! to be hand-wired:
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
pub mod manager;
pub mod providers;

pub use builtin::{
    built_in_provider_defaults, default_anthropic, default_claude_cli, default_gemini,
    default_gemini_cli, default_openai, merge_builtin_with_user,
};
pub use error::ConfigError;
pub use manager::{Config, ConfigManager};
pub use providers::{ProviderConfig, ProvidersConfig};
