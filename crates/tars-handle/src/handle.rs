//! The [`Tars`] per-scope handle (Doc 06 §6 C3): the single entry that binds
//! the global provider registry + the workspace roles + a per-scope
//! observability sink + a cancellation token for one workspace (local) or
//! tenant×workspace (server).
//!
//! Two config layers meet here: the *global* registry (built once from
//! `~/.tars/config.toml`) and the *workspace* roles
//! (`<root>/.<tool>/config.toml` `[roles]`). Role resolution walks
//! workspace → provider id → global registry.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use tars_config::RoutingConfig;
use tars_pipeline::{EventStores, Pipeline, PipelineOpts};
use tars_provider::{LlmProvider, ProviderRegistry};
use tars_runtime::{LocalRuntime, Runtime};
use tars_storage::{
    BodyStore, EventStore, PipelineEventStore, SqliteBodyStore, SqliteBodyStoreConfig,
    SqliteEventStore, SqlitePipelineEventStore, SqlitePipelineEventStoreConfig,
    open_event_store_at_path,
};
use tars_types::{CancellationToken, ModelTier, ProviderId, SessionId};

use crate::error::TarsError;
use crate::paths::{
    StoreScope, standalone_store_dir, tars_home_store_dir, workspace_store_dir,
};

/// Per-scope runtime handle. Cheap to hold — the registry is a shared `Arc`,
/// the roles a small map, the stores `Arc` handles. Dropping it cancels the
/// scope and releases its stores (see [`Drop`]).
pub struct Tars {
    registry: Arc<ProviderRegistry>,
    roles: RoutingConfig,
    /// Per-scope stores. Task 4 consolidates these three separate SQLite
    /// stores behind one MPSC single-writer sink; today they are reused
    /// directly so pipeline + runtime can emit.
    sink: Sink,
    cancel: CancellationToken,
    root: PathBuf,
    scope: StoreScope,
}

/// The per-scope stores. `runtime` (the DAG event store, `LocalRuntime`) is
/// always present; `pipeline` (the observability pair) is absent when the
/// scope opted out (`StoreScope::Off`).
struct Sink {
    // Task 4: the real consolidated MPSC single-writer sink replaces these.
    runtime: Arc<dyn EventStore>,
    pipeline: Option<PipelineSink>,
}

struct PipelineSink {
    events: Arc<dyn PipelineEventStore>,
    bodies: Arc<dyn BodyStore>,
}

/// Workspace-layer config: `<root>/.<tool>/config.toml`. Only `[roles]`
/// (a [`RoutingConfig`]: tier → provider ids) and the `[store]` placement
/// knob live here — never secrets. Unknown sections are ignored so a
/// consumer tool can keep its own config alongside.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WorkspaceConfig {
    roles: RoutingConfig,
    store: StoreSettings,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct StoreSettings {
    /// `false` ⇒ [`StoreScope::Off`].
    enabled: bool,
    /// `"tars_home"` forces the `~/.tars` fallback even for a writable
    /// workspace.
    location: Option<String>,
}

impl Default for StoreSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            location: None,
        }
    }
}

impl Tars {
    /// Build a handle for an already-resolved workspace `root` (§6 C3).
    ///
    /// `root` is canonicalized; roles load from `<root>/.<tool>/config.toml`
    /// (default if absent); the store scope resolves per §7 and the
    /// `<root>/.<tool>/tars/` dir is bootstrapped on first use. Requires
    /// [`Config::load`] to have run (the global registry is built from it).
    pub fn for_workspace(tool: &str, root: &Path) -> Result<Tars, TarsError> {
        let root = root.canonicalize()?;
        let registry = ProviderRegistry::global()?;
        let ws_cfg = load_workspace_config(tool, &root)?;
        let scope = resolve_scope(tool, &root, &ws_cfg)?;
        let sink = open_sink(&scope)?;
        Ok(Tars {
            registry,
            roles: ws_cfg.roles,
            sink,
            cancel: CancellationToken::new(),
            root,
            scope,
        })
    }

    /// Build a handle with no workspace (§7 standalone): store lives under
    /// `~/.tars/standalone/<tool>/<session>/`, partitioned by session.
    pub fn standalone(tool: &str, session: SessionId) -> Result<Tars, TarsError> {
        let registry = ProviderRegistry::global()?;
        let home = tars_home()?;
        let dir = standalone_store_dir(&home, tool, session.as_str());
        let scope = StoreScope::TarsHome(dir);
        let sink = open_sink(&scope)?;
        let root = scope.dir().map(Path::to_path_buf).unwrap_or_default();
        Ok(Tars {
            registry,
            roles: RoutingConfig::default(),
            sink,
            cancel: CancellationToken::new(),
            root,
            scope,
        })
    }

    /// Resolve `role` → a live provider (§6 C3). Fallback chain:
    /// (1) `role` as a tier → its first candidate; (2) `role` as a literal
    /// provider id; (3) the `default` tier's first candidate; (4) the sole
    /// provider if the registry has exactly one; else [`TarsError::UnknownRole`].
    pub fn provider(&self, role: &str) -> Result<Arc<dyn LlmProvider>, TarsError> {
        Ok(self.resolve(role)?.1)
    }

    /// Build the canonical pipeline for `role`, with this scope's sink wired
    /// into the `EventEmitter` layer (`None` sink ⇒ no emission).
    pub fn pipeline(&self, role: &str) -> Result<Pipeline, TarsError> {
        let (id, provider) = self.resolve(role)?;
        let mut opts = PipelineOpts::new(id);
        if let Some(p) = &self.sink.pipeline {
            opts.events = Some(EventStores {
                events: p.events.clone(),
                bodies: p.bodies.clone(),
            });
        }
        Ok(Pipeline::default_chain(provider, opts))
    }

    /// The DAG runtime for this scope, ready to pass to
    /// [`tars_runtime::run_plan`]. Backed by this scope's event store.
    pub fn runtime(&self) -> Arc<dyn Runtime> {
        LocalRuntime::new(self.sink.runtime.clone())
    }

    /// Cancel the scope's work. Fire before dropping the handle on close so a
    /// hung job can't pin it (§10). Idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The scope's cancellation token — thread its `.child_token()` into
    /// [`tars_runtime::run_plan`] so [`Tars::cancel`] / `Drop` cancels the run.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// The canonical workspace root this handle is bound to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where this scope's store landed (workspace / tars_home / off).
    pub fn store_scope(&self) -> &StoreScope {
        &self.scope
    }

    fn resolve(
        &self,
        role: &str,
    ) -> Result<(ProviderId, Arc<dyn LlmProvider>), TarsError> {
        // 1. role names a tier → first candidate in that tier.
        if let Some(tier) = parse_tier(role) {
            if let Some(hit) = self.first_in_tier(&tier) {
                return Ok(hit);
            }
        }
        // 2. role is a literal provider id.
        let literal = ProviderId::new(role);
        if let Some(p) = self.registry.get(&literal) {
            return Ok((literal, p));
        }
        // 3. fall back to the `default` tier.
        if let Some(hit) = self.first_in_tier(&ModelTier::Default) {
            return Ok(hit);
        }
        // 4. a single-provider registry has an unambiguous answer.
        if self.registry.len() == 1 {
            if let Some(id) = self.registry.ids().next().cloned() {
                if let Some(p) = self.registry.get(&id) {
                    return Ok((id, p));
                }
            }
        }
        Err(TarsError::UnknownRole {
            role: role.to_string(),
            tried: Some(literal),
        })
    }

    fn first_in_tier(
        &self,
        tier: &ModelTier,
    ) -> Option<(ProviderId, Arc<dyn LlmProvider>)> {
        let id = self.roles.tiers.get(tier)?.first()?;
        let provider = self.registry.get(id)?;
        Some((id.clone(), provider))
    }
}

/// Deterministic Drop (§10): cancel, then release the stores. When the last
/// `Arc` to each store drops its SQLite connection closes. Task 4 replaces
/// this with an MPSC writer drain.
impl Drop for Tars {
    fn drop(&mut self) {
        self.cancel.cancel();
        // `self.sink` drops here → store `Arc`s release. In-flight jobs that
        // still hold their own clones keep their store alive until they finish.
    }
}

/// Map a role string to a [`ModelTier`] (case-insensitive). Deviation from
/// Doc 06 recorded in the crate docs: `RoutingConfig` keys on the fixed
/// `ModelTier` enum, so an arbitrary role must name one of these tiers (or
/// fall through to the literal-provider-id path in [`Tars::resolve`]).
fn parse_tier(role: &str) -> Option<ModelTier> {
    match role.to_ascii_lowercase().as_str() {
        "reasoning" => Some(ModelTier::Reasoning),
        "default" => Some(ModelTier::Default),
        "fast" => Some(ModelTier::Fast),
        "local" => Some(ModelTier::Local),
        _ => None,
    }
}

fn tars_home() -> Result<PathBuf, TarsError> {
    tars_config::resolve_home(None).ok_or_else(|| {
        TarsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no tars home directory (set $TARS_HOME or ensure a home dir)",
        ))
    })
}

fn load_workspace_config(tool: &str, root: &Path) -> Result<WorkspaceConfig, TarsError> {
    let path = root.join(format!(".{tool}")).join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(toml::from_str(&s)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(WorkspaceConfig::default()),
        Err(e) => Err(e.into()),
    }
}

fn resolve_scope(
    tool: &str,
    root: &Path,
    cfg: &WorkspaceConfig,
) -> Result<StoreScope, TarsError> {
    if !cfg.store.enabled {
        return Ok(StoreScope::Off);
    }
    if cfg.store.location.as_deref() == Some("tars_home") {
        let home = tars_home()?;
        return Ok(StoreScope::TarsHome(tars_home_store_dir(&home, root)));
    }
    let dir = workspace_store_dir(tool, root);
    // Writable? Bootstrapping the dir is the writability probe: a read-only
    // workspace falls back to the tars-home store (§7).
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Ok(StoreScope::Workspace(dir)),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let home = tars_home()?;
            Ok(StoreScope::TarsHome(tars_home_store_dir(&home, root)))
        }
        Err(e) => Err(e.into()),
    }
}

fn open_sink(scope: &StoreScope) -> Result<Sink, TarsError> {
    match scope {
        StoreScope::Off => {
            // No persistent store: the runtime still needs an event store, so
            // give it a throwaway in-memory one; no pipeline emission.
            let runtime: Arc<dyn EventStore> = SqliteEventStore::in_memory()?;
            Ok(Sink {
                runtime,
                pipeline: None,
            })
        }
        StoreScope::Workspace(dir) | StoreScope::TarsHome(dir) => {
            std::fs::create_dir_all(dir)?;
            let runtime: Arc<dyn EventStore> =
                open_event_store_at_path(&dir.join("events.sqlite"))?;
            let events: Arc<dyn PipelineEventStore> = SqlitePipelineEventStore::open(
                SqlitePipelineEventStoreConfig::new(dir.join("pipeline_events.sqlite")),
            )?;
            let bodies: Arc<dyn BodyStore> =
                SqliteBodyStore::open(SqliteBodyStoreConfig::new(dir.join("bodies.sqlite")))?;
            Ok(Sink {
                runtime,
                pipeline: Some(PipelineSink { events, bodies }),
            })
        }
    }
}

/// Consumer lifecycle pattern (§10): a `Mutex<HashMap<root, Tars>>` — no
/// LRU/TTL, explicit open/switch/close. `open_workspace` inserts on first
/// touch; `switch` just gets/creates B (the global registry is never
/// rebuilt); `close` removes + cancels (Drop follows once in-flight jobs
/// release their `Arc`s).
///
/// ```no_run
/// use std::collections::HashMap;
/// use std::path::{Path, PathBuf};
/// use std::sync::Mutex;
/// use tars_handle::{Tars, WorkspaceHandles, resolve_workspace_root, WorkspaceResolution};
///
/// let handles: WorkspaceHandles = Mutex::new(HashMap::new());
///
/// // open / switch
/// # fn demo(handles: &WorkspaceHandles, entry: &Path) -> Result<(), Box<dyn std::error::Error>> {
/// if let WorkspaceResolution::Workspace(root) = resolve_workspace_root("arc", entry)? {
///     let mut map = handles.lock().unwrap();
///     map.entry(root.clone())
///         .or_insert_with(|| Tars::for_workspace("arc", &root).expect("open"));
/// }
/// # Ok(()) }
///
/// // close
/// # fn close(handles: &WorkspaceHandles, root: &Path) {
/// if let Some(t) = handles.lock().unwrap().remove(root) {
///     t.cancel(); // Drop drains after in-flight jobs release their Arc
/// }
/// # }
/// ```
pub type WorkspaceHandles = std::sync::Mutex<std::collections::HashMap<PathBuf, Tars>>;
