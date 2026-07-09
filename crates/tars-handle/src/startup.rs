//! Composition-root entry (Doc 06 §C1/§C2): install the process-global config
//! and build the one provider registry, so `Config::get()` and
//! `ProviderRegistry::global()` work for the rest of the process (tars
//! internals *and* the embedding app).
//!
//! Two entries, one body:
//! - [`init`] is the DI form — the host app owns config discovery (its own
//!   file/env) and hands tars the [`Config`] it should consume.
//! - [`init_from_home`] is the convenience form for the `tars` CLI, which has
//!   no host app: it reads `<home>/config.toml` off disk, then does the same.
//!
//! Both are first-wins on the underlying singletons (mirroring
//! [`Config::load`]); the facade additionally reports a *second* init as
//! [`InitError::AlreadyInitialized`] so a mis-sequenced embedder finds out
//! rather than silently running against a stale config.

use std::path::PathBuf;
use std::sync::OnceLock;

use thiserror::Error;

use tars_config::Config;
use tars_provider::{ProviderRegistry, RegistryError};

/// Set once by [`init`] / [`init_from_home`]. The config + registry cells are
/// themselves first-wins; this guard is only so a *second* init call is
/// reported instead of silently ignored.
static INIT: OnceLock<()> = OnceLock::new();

/// Failure at the composition root.
#[derive(Debug, Error)]
pub enum InitError {
    /// Loading / parsing `<home>/config.toml` failed ([`init_from_home`] only).
    #[error("config load: {0}")]
    Config(#[from] tars_config::ConfigError),
    /// Building the one provider registry from the config failed (bad provider
    /// declaration, missing feature, HTTP base init).
    #[error("provider registry: {0}")]
    Registry(#[from] RegistryError),
    /// `init` / `init_from_home` already ran in this process.
    #[error("tars already initialized")]
    AlreadyInitialized,
}

/// DI entry (embedders): install `config` as the process-global config and
/// eagerly build the one provider registry from it.
///
/// The host app loads config from wherever it likes and passes the
/// tars-relevant [`Config`] in; tars consumes it — it does not discover config
/// itself. After this returns, `Config::get()` and
/// [`ProviderRegistry::global`] are live for the whole process.
///
/// Building the registry eagerly means a bad provider / key surfaces *here*,
/// at startup, rather than on the first request. First-wins: if the config
/// singleton was already installed (e.g. a prior `init`), that install stays
/// and this call reports [`InitError::AlreadyInitialized`].
pub fn init(config: Config) -> Result<(), InitError> {
    if INIT.get().is_some() {
        return Err(InitError::AlreadyInitialized);
    }
    Config::set(config);
    let _ = ProviderRegistry::global()?;
    let _ = INIT.set(());
    Ok(())
}

/// Convenience entry (the `tars` CLI): read `<home>/config.toml` off disk
/// (`home` > `$TARS_HOME` > `~/.tars`), then the same as [`init`].
pub fn init_from_home(home: Option<PathBuf>) -> Result<(), InitError> {
    if INIT.get().is_some() {
        return Err(InitError::AlreadyInitialized);
    }
    Config::load(home)?;
    let _ = ProviderRegistry::global()?;
    let _ = INIT.set(());
    Ok(())
}

/// Whether [`init`] / [`init_from_home`] has already run in this process.
pub fn is_initialized() -> bool {
    INIT.get().is_some()
}
