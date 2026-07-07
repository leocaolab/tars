//! `tars-handle` — the [`Tars`] per-scope runtime handle (Doc 06 §6 C3).
//!
//! The missing single entry that binds tars's global layer (immutable
//! [`Config`](tars_config::Config) + built-once
//! [`ProviderRegistry`](tars_provider::ProviderRegistry)) to a *scope*
//! (one workspace locally / one tenant×workspace on a server):
//!
//! ```text
//! Config::load(home)                      // once, at the composition root
//! let tars = Tars::for_workspace("arc", &root)?;   // per workspace
//! let pipeline = tars.pipeline("default")?;         // one agent call
//! let runtime  = tars.runtime();                    // DAG via run_plan
//! ```
//!
//! ## Role resolution
//!
//! The workspace `[roles]` table is a **flat** map of arbitrary role name →
//! provider id — the shape real consumers already write (`arc`'s
//! `.arc/config.toml`, `concer`'s `.concer/config.toml`):
//!
//! ```toml
//! [roles]
//! critic = "deepseek"
//! fixer  = "claude_cli"
//! ```
//!
//! `provider(role: &str)` resolves in order: (1) the flat `[roles]` map
//! (workspace entries overlaid on the global `[roles]`) → provider id → global
//! registry; else (2) `role` naming a fixed
//! [`ModelTier`](tars_types::ModelTier) (`reasoning` / `default` / `fast` /
//! `local`) via the global tier [`RoutingConfig`]; else (3) `role` as a literal
//! provider id; else (4) the `default` tier; else (5) the sole provider; else
//! [`TarsError::UnknownRole`]. Tier routing stays a separate concern owned by
//! the global config's `[routing.tiers]`
//! ([`RoutingConfig`](tars_config::RoutingConfig)), not the workspace
//! `[roles]`.

pub mod error;
pub mod handle;
pub mod paths;
mod resilience;

pub use error::TarsError;
pub use handle::{Tars, WorkspaceHandles};
pub use paths::{
    StoreScope, WorkspaceResolution, resolve_workspace_root, standalone_store_dir,
    tars_home_store_dir, workspace_store_dir,
};
