//! `tars-handle` — the slim composition-root facade.
//!
//! After the scope-facade simplification this crate is a thin bundle of
//! standalone pieces, not the old per-scope `Tars` handle:
//!
//! - [`init`] / [`init_from_home`] — install the process-global
//!   [`Config`](tars_config::Config) + build the one
//!   [`ProviderRegistry`](tars_provider::ProviderRegistry).
//! - [`resolve_role`] — role → provider against an explicit registry + routing
//!   + `[roles]` map (no global, no scope; every input a plain argument).
//! - [`paths`] — plain "where does the store dir / workspace root live" helpers.
//! - [`resilience_configs`] — bridge the `[resilience]` config onto the
//!   pipeline's retry / circuit-breaker knobs.
//!
//! ## Role resolution
//!
//! The `[roles]` table is a **flat** map of arbitrary role name → provider id —
//! the shape real consumers already write (`arc`'s `.arc/config.toml`,
//! `concer`'s `.concer/config.toml`):
//!
//! ```toml
//! [roles]
//! critic = "deepseek"
//! fixer  = "claude_cli"
//! ```
//!
//! [`resolve_role`] resolves in order: (1) the flat `[roles]` map → provider id
//! → registry; else (2) `role` naming a fixed
//! [`ModelTier`](tars_types::ModelTier) (`reasoning` / `default` / `fast` /
//! `local`) via the tier [`RoutingConfig`](tars_config::RoutingConfig); else
//! (3) `role` as a literal provider id; else (4) the `default` tier; else
//! (5) the sole provider; else [`TarsError::UnknownRole`].

pub mod error;
pub mod paths;
pub mod resilience;
pub mod roles;
pub mod startup;

pub use error::TarsError;
pub use resilience::resilience_configs;
pub use roles::{
    parse_tier, resolve_provider_id, resolve_role, resolve_role_bound, resolve_service,
};
pub use startup::{InitError, init, init_from_home, is_initialized};
pub use paths::{
    StoreScope, WorkspaceResolution, resolve_workspace_root, standalone_store_dir,
    tars_home_store_dir, workspace_store_dir,
};
