//! Typed errors for the [`Tars`](crate::Tars) handle.
//!
//! Every underlying error is carried **typed** via `#[from]` — callers branch
//! on the variant, never a stringified `.contains()` grep.

use thiserror::Error;

use tars_types::ProviderId;

#[derive(Debug, Error)]
pub enum TarsError {
    /// Building / consulting the global provider registry failed.
    #[error("provider registry: {0}")]
    Registry(#[from] tars_provider::RegistryError),

    /// Opening or writing the per-scope recovery event store
    /// (`AgentEventLog`, tars-storage) failed.
    #[error("store: {0}")]
    Storage(#[from] tars_storage::StorageError),

    /// Opening or writing a per-scope observability store
    /// (`PipelineEventLog` / `LlmRecordStore`, `tars_melt::event`)
    /// failed.
    #[error("event store: {0}")]
    MeltStore(#[from] tars_melt::event::StoreError),

    /// The workspace `<root>/.<tool>/config.toml` could not be parsed.
    #[error("workspace config parse: {0}")]
    WorkspaceConfig(#[from] toml::de::Error),

    /// Filesystem error resolving / bootstrapping a workspace path.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A `role` did not resolve to any provider — not in the flat `[roles]`
    /// map, not a known tier, not a literal provider id, no `default` tier
    /// candidate, and the registry isn't a single-provider registry to fall
    /// back to. Carries the role and the resolved provider id (if any) so the
    /// message is actionable.
    #[error(
        "role `{role}` maps to no provider — add a `[roles]` entry \
         (`{role} = \"<provider>\"`), name a provider id directly, or declare a \
         `default` tier{}",
        .tried.as_ref().map(|p| format!(" (tried provider id `{p}`)")).unwrap_or_default()
    )]
    UnknownRole {
        role: String,
        tried: Option<ProviderId>,
    },
}
