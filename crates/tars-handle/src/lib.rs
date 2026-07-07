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
//! ## Deviation from Doc 06 (recorded)
//!
//! Doc 06 §6 C3 types the handle's roles as [`RoutingConfig`] and exposes
//! `provider(role: &str)`. `RoutingConfig` keys on the fixed four-value
//! [`ModelTier`](tars_types::ModelTier) enum (`reasoning` / `default` /
//! `fast` / `local`), **not** arbitrary role strings. So a `role` resolves
//! by: (1) naming a tier → that tier's first candidate; else (2) naming a
//! provider id literally; else (3) the `default` tier; else (4) the sole
//! provider. The workspace `[roles]` table therefore deserializes as a
//! `RoutingConfig` (`[roles.tiers]`, tier → provider ids). This is the
//! minimal adaptation that keeps the cited type; no new role abstraction was
//! invented.

pub mod error;
pub mod handle;
pub mod paths;

pub use error::TarsError;
pub use handle::{Tars, WorkspaceHandles};
pub use paths::{
    StoreScope, WorkspaceResolution, resolve_workspace_root, standalone_store_dir,
    tars_home_store_dir, workspace_store_dir,
};
