//! The [`Tars`] per-scope handle (Doc 06 §6 C3): the single entry that binds
//! the global provider registry + the workspace roles + a per-scope
//! observability sink + a cancellation token for one workspace (local) or
//! tenant×workspace (server).
//!
//! Two config layers meet here: the *global* config (built once from
//! `~/.tars/config.toml` — its provider registry + tier `routing` + any global
//! `[roles]`) and the *workspace* `[roles]`
//! (`<root>/.<tool>/config.toml`, a flat `name → provider id` map). Role
//! resolution walks workspace `[roles]` → global `[roles]` → tier → literal id
//! → default tier → sole provider → global registry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use tars_config::{Config, ProvidersConfig, ResilienceConfig, RoutingConfig};
use tars_pipeline::{EventStores, OutputValidator, Pipeline, PipelineOpts};
use tars_provider::{HttpProviderBase, LlmProvider, ProviderRegistry};
use tars_runtime::{LocalRuntime, Runtime};
use tars_storage::{
    BodyStore, EventStore, PipelineEventStore, SqliteBodyStore, SqliteBodyStoreConfig,
    SqliteEventStore, SqlitePipelineEventStore, SqlitePipelineEventStoreConfig,
    open_event_store_at_path,
};
use tars_types::{Capabilities, CancellationToken, ModelTier, ProviderId, SessionId};

use crate::error::TarsError;
use crate::paths::{
    StoreScope, standalone_store_dir, tars_home_store_dir, workspace_store_dir,
};

/// Per-scope runtime handle. Cheap to hold — the registry is a shared `Arc`,
/// the roles a small map, the stores `Arc` handles. Dropping it cancels the
/// scope and releases its stores (see [`Drop`]).
pub struct Tars {
    registry: Arc<ProviderRegistry>,
    /// Flat `[roles]` map (arbitrary name → provider id): workspace entries
    /// overlaid on the global `[roles]`. Consulted first in [`Tars::resolve`].
    roles: HashMap<String, ProviderId>,
    /// Tier-based routing (`ModelTier → provider ids`) from the global config;
    /// the fallback after the flat `roles` map misses.
    routing: RoutingConfig,
    /// `[resilience]` tuning from the global config — retry + circuit-breaker
    /// policy fed into every pipeline this handle builds. Snapshotted at
    /// construction (like `roles`/`routing`) so `pipeline_with` never touches
    /// the global singleton. Default (both `None`) ⇒ tars's current pipeline
    /// (default retry, no breaker).
    resilience: ResilienceConfig,
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
/// (a **flat** `name → provider id` map — the shape `arc`/`concer` already
/// write) and the `[store]` placement knob live here — never secrets. Unknown
/// sections are ignored so a consumer tool can keep its own config alongside.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WorkspaceConfig {
    roles: HashMap<String, ProviderId>,
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

/// Where a pipeline's observability events go. The default ([`Scope`]) uses
/// the handle's own per-scope sink (the common path — concer). A consumer with
/// its own store policy (e.g. arc keeps bodies OFF in release so it never
/// persists the reviewed repo's proprietary source) supplies [`Use`], or opts a
/// pipeline out of emission entirely with [`Off`].
///
/// [`Scope`]: EventsOverride::Scope
/// [`Use`]: EventsOverride::Use
/// [`Off`]: EventsOverride::Off
#[derive(Default)]
pub enum EventsOverride {
    /// Wire the handle's own scope sink (no emission if the scope is `Off`).
    #[default]
    Scope,
    /// Emit nothing for this pipeline, regardless of the scope sink.
    Off,
    /// Use these consumer-provided stores instead of the scope sink.
    Use(EventStores),
}

/// Consumer-supplied overrides for [`Tars::pipeline_with_overrides`]. Every
/// field defaults to the plain-`pipeline` behavior, so `Default::default()`
/// reproduces [`Tars::pipeline`] exactly.
#[derive(Default)]
pub struct PipelineOverrides {
    /// Output validators layered into the pipeline's `ValidationMiddleware`
    /// (empty ⇒ no validation layer).
    pub validators: Vec<Arc<dyn OutputValidator>>,
    /// Where this pipeline's events go (see [`EventsOverride`]).
    pub events: EventsOverride,
    /// Override the resolved provider's per-subprocess `timeout_secs`. `Some`
    /// forces a FRESH provider built with this timeout (not the warm shared
    /// one) — the rare big-budget path. `None` ⇒ the shared warm provider.
    pub timeout_secs: Option<u64>,
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
        // Global config supplies tier `routing` + any global `[roles]`; the
        // workspace `[roles]` overlay (same name) wins. `Config::get` is safe
        // here — `ProviderRegistry::global` above already required it loaded.
        let global = Config::get();
        let mut roles = global.roles.clone();
        roles.extend(ws_cfg.roles);
        Ok(Tars {
            registry,
            roles,
            routing: global.routing.clone(),
            resilience: global.resilience.clone(),
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
        // No workspace overlay standalone: just the global `[roles]` + routing.
        let global = Config::get();
        Ok(Tars {
            registry,
            roles: global.roles.clone(),
            routing: global.routing.clone(),
            resilience: global.resilience.clone(),
            sink,
            cancel: CancellationToken::new(),
            root,
            scope,
        })
    }

    /// Resolve `role` → a live provider (§6 C3). Resolution order:
    /// (1) the flat `[roles]` map (`role` → provider id) — highest priority;
    /// (2) `role` as a tier → its first candidate; (3) `role` as a literal
    /// provider id; (4) the `default` tier's first candidate; (5) the sole
    /// provider if the registry has exactly one; else [`TarsError::UnknownRole`].
    pub fn provider(&self, role: &str) -> Result<Arc<dyn LlmProvider>, TarsError> {
        Ok(self.resolve(role)?.1)
    }

    /// Build the canonical pipeline for `role`, with this scope's sink wired
    /// into the `EventEmitter` layer (`None` sink ⇒ no emission).
    ///
    /// Thin wrapper over [`Tars::pipeline_with_overrides`] with all defaults
    /// (no validators, scope sink, no timeout override); the resolved
    /// provider's [`Capabilities`] are dropped. Reach for `pipeline_with` /
    /// `pipeline_with_overrides` when the consumer needs to inject validators,
    /// its own event stores, or a per-call timeout, or wants the capabilities.
    pub fn pipeline(&self, role: &str) -> Result<Pipeline, TarsError> {
        Ok(self
            .pipeline_with_overrides(role, PipelineOverrides::default())?
            .0)
    }

    /// Build the canonical pipeline for `role` with `validators` layered in,
    /// and return it alongside the resolved provider's [`Capabilities`].
    ///
    /// Thin wrapper over [`Tars::pipeline_with_overrides`] carrying only the
    /// validators (scope sink, no timeout override). The extra `validators`
    /// become the pipeline's `ValidationMiddleware` (empty ⇒ no validation
    /// layer, identical to `pipeline`), and the provider's declared
    /// [`Capabilities`] are surfaced so a consumer can run a structured-output
    /// / tool-use preflight before dispatching.
    pub fn pipeline_with(
        &self,
        role: &str,
        validators: Vec<Arc<dyn OutputValidator>>,
    ) -> Result<(Pipeline, Capabilities), TarsError> {
        self.pipeline_with_overrides(
            role,
            PipelineOverrides {
                validators,
                ..Default::default()
            },
        )
    }

    /// Build the canonical pipeline for `role`, letting the consumer inject its
    /// own policy via [`PipelineOverrides`]: output validators, an event-store
    /// override (its own bodies policy, or emit-nothing), and a per-call
    /// subprocess-timeout override. Returns the pipeline alongside the resolved
    /// provider's [`Capabilities`].
    ///
    /// Role→provider resolution and the `[resilience]` wiring are the same as
    /// [`Tars::pipeline`]. The registry (and thus the warm provider) is built
    /// ONCE for this handle, so repeated `None`-timeout calls reuse one warm
    /// provider `Arc` — the whole point of the shared handle. A `Some` timeout
    /// override is the exception: it builds a FRESH provider from the config
    /// with `timeout_secs` patched (the timeout is baked in at construction, so
    /// re-timing the warm shared one is impossible) — the rare big-budget path,
    /// paying one build.
    pub fn pipeline_with_overrides(
        &self,
        role: &str,
        overrides: PipelineOverrides,
    ) -> Result<(Pipeline, Capabilities), TarsError> {
        let (id, provider) = match overrides.timeout_secs {
            Some(secs) => self.resolve_with_timeout(role, secs)?,
            None => self.resolve(role)?,
        };
        let capabilities = provider.capabilities().clone();
        let mut opts = PipelineOpts::new(id);
        opts.validators = overrides.validators;
        // Feed the global `[resilience]` section into the pipeline's retry +
        // circuit-breaker knobs. Both `None` (no `[resilience]` config) ⇒
        // `default_chain` produces exactly today's chain (default retry, no
        // breaker); a populated section overrides.
        let (retry, circuit_breaker) = crate::resilience::resilience_configs(&self.resilience);
        opts.retry = retry;
        opts.circuit_breaker = circuit_breaker;
        match overrides.events {
            EventsOverride::Scope => {
                if let Some(p) = &self.sink.pipeline {
                    opts.events = Some(EventStores {
                        events: p.events.clone(),
                        bodies: p.bodies.clone(),
                    });
                }
            }
            EventsOverride::Off => {}
            EventsOverride::Use(stores) => opts.events = Some(stores),
        }
        Ok((Pipeline::default_chain(provider, opts), capabilities))
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
        // 1. flat `[roles]` map: arbitrary name → provider id (workspace over
        //    global). Highest priority — this is the shape arc/concer write.
        if let Some(id) = self.roles.get(role) {
            if let Some(p) = self.registry.get(id) {
                return Ok((id.clone(), p));
            }
        }
        // 2. role names a tier → first candidate in that tier.
        if let Some(tier) = parse_tier(role) {
            if let Some(hit) = self.first_in_tier(&tier) {
                return Ok(hit);
            }
        }
        // 3. role is a literal provider id.
        let literal = ProviderId::new(role);
        if let Some(p) = self.registry.get(&literal) {
            return Ok((literal, p));
        }
        // 4. fall back to the `default` tier.
        if let Some(hit) = self.first_in_tier(&ModelTier::Default) {
            return Ok(hit);
        }
        // 5. a single-provider registry has an unambiguous answer.
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
        let id = self.routing.tiers.get(tier)?.first()?;
        let provider = self.registry.get(id)?;
        Some((id.clone(), provider))
    }

    /// Resolve `role` → its provider id (the normal way), then build a FRESH
    /// provider for just that id with `timeout_secs` patched. Not the warm
    /// shared `Arc` — the per-subprocess timeout is baked in at construction,
    /// so a bigger-budget call needs its own provider instance. Rare path (one
    /// long merge/reconcile turn), so paying one build is fine.
    fn resolve_with_timeout(
        &self,
        role: &str,
        secs: u64,
    ) -> Result<(ProviderId, Arc<dyn LlmProvider>), TarsError> {
        let (id, shared) = self.resolve(role)?;
        // Providers resolved through the *global* registry were built from the
        // global config, so the id is present there. A synthetic handle (tests)
        // may carry a registry with no matching global config entry — fall back
        // to the shared provider rather than panic in `Config::get`.
        let pc = if Config::is_loaded() {
            Config::get().providers.get(&id).cloned()
        } else {
            None
        };
        let Some(pc) = pc else {
            return Ok((id, shared));
        };
        let patched = pc.with_timeout_secs(secs);
        let one = ProvidersConfig::from_map(HashMap::from([(id.clone(), patched)]));
        let http = HttpProviderBase::default_arc()
            .map_err(|e| tars_provider::RegistryError::HttpBaseInit(e.to_string()))?;
        let registry = ProviderRegistry::from_config(&one, http, tars_provider::basic())?;
        let provider = registry
            .get(&id)
            .ok_or_else(|| TarsError::UnknownRole {
                role: role.to_string(),
                tried: Some(id.clone()),
            })?;
        Ok((id, provider))
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

/// Map a role string to a [`ModelTier`] (case-insensitive). This is only the
/// *tier* fallback in [`Tars::resolve`]: an arbitrary role name is served first
/// by the flat `[roles]` map, and only if it misses does the role get matched
/// against these fixed tier names (`RoutingConfig` keys on the `ModelTier`
/// enum, not arbitrary strings).
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

#[cfg(test)]
mod tests {
    use super::*;
    use tars_config::ConfigManager;
    use tars_pipeline::NotEmptyValidator;
    use tars_provider::{HttpProviderBase, basic};

    /// Build a handle around a hand-rolled registry holding a single `mock`
    /// provider, with the store scope Off (no network, no on-disk sink). Same
    /// module ⇒ we can construct `Tars` directly and skip the global
    /// `Config::load` composition root the real constructors require.
    fn mock_handle() -> Tars {
        let cfg = ConfigManager::load_from_str(
            r#"
            [providers.mock]
            type = "mock"
            canned_response = "hi"
            "#,
        )
        .expect("parse mock config");
        let http = HttpProviderBase::default_arc().expect("http base");
        let registry = Arc::new(
            ProviderRegistry::from_config(&cfg.providers, http, basic())
                .expect("build registry"),
        );
        Tars {
            registry,
            roles: HashMap::new(),
            routing: RoutingConfig::default(),
            resilience: ResilienceConfig::default(),
            sink: open_sink(&StoreScope::Off).expect("open off sink"),
            cancel: CancellationToken::new(),
            root: PathBuf::from("."),
            scope: StoreScope::Off,
        }
    }

    /// Like [`mock_handle`] but every provider call fails (via `err`), and
    /// the handle carries `resilience`. Returns the handle + a shared hit
    /// counter so a test can observe how many times the provider was reached
    /// (i.e. whether the retry / breaker config actually took effect). Built
    /// by mapping the config's `mock` provider to an always-erroring one so
    /// the role still resolves to id `mock`.
    fn erroring_handle(
        resilience: ResilienceConfig,
        err: fn() -> tars_types::ProviderError,
    ) -> (Tars, Arc<std::sync::atomic::AtomicU32>) {
        use tars_provider::LlmEventStream;
        use tars_types::{Capabilities, ChatRequest, ProviderError, RequestContext};

        struct AlwaysErr {
            id: ProviderId,
            caps: Capabilities,
            hits: Arc<std::sync::atomic::AtomicU32>,
            err: fn() -> ProviderError,
        }
        #[async_trait::async_trait]
        impl LlmProvider for AlwaysErr {
            fn id(&self) -> &ProviderId {
                &self.id
            }
            fn capabilities(&self) -> &Capabilities {
                &self.caps
            }
            async fn stream(
                self: Arc<Self>,
                _req: ChatRequest,
                _ctx: RequestContext,
            ) -> Result<LlmEventStream, ProviderError> {
                self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err((self.err)())
            }
        }

        let cfg = ConfigManager::load_from_str(
            "[providers.mock]\ntype = \"mock\"\ncanned_response = \"hi\"\n",
        )
        .expect("parse mock config");
        let http = HttpProviderBase::default_arc().expect("http base");
        let base = ProviderRegistry::from_config(&cfg.providers, http, basic())
            .expect("build registry");
        let hits = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hits_for_map = hits.clone();
        // Replace the config-built mock with an always-erroring provider under
        // the same id, so `resolve("mock")` still finds it.
        let registry = Arc::new(base.map_providers(|id, p| {
            Arc::new(AlwaysErr {
                id: id.clone(),
                caps: p.capabilities().clone(),
                hits: hits_for_map.clone(),
                err,
            }) as Arc<dyn LlmProvider>
        }));

        let tars = Tars {
            registry,
            roles: HashMap::new(),
            routing: RoutingConfig::default(),
            resilience,
            sink: open_sink(&StoreScope::Off).expect("open off sink"),
            cancel: CancellationToken::new(),
            root: PathBuf::from("."),
            scope: StoreScope::Off,
        };
        (tars, hits)
    }

    #[test]
    fn pipeline_with_layers_validators_and_surfaces_capabilities() {
        let tars = mock_handle();
        let validators: Vec<Arc<dyn OutputValidator>> =
            vec![Arc::new(NotEmptyValidator::new())];

        let (pipeline, caps) = tars
            .pipeline_with("mock", validators)
            .expect("build validated pipeline");

        // The injected validator must materialize as a `validation` layer in
        // the onion (sink is Off ⇒ no `event_emitter`).
        assert!(
            pipeline.layer_names().contains(&"validation"),
            "validation layer missing from chain: {:?}",
            pipeline.layer_names(),
        );

        // Capabilities came from the resolved provider — a plausible, real
        // struct (the mock's text-only baseline), not a fabricated default.
        assert!(caps.max_context_tokens > 0, "capabilities look empty: {caps:?}");
    }

    #[test]
    fn pipeline_with_overrides_timeout_builds_valid_pipeline() {
        // A `Some` timeout override takes the fresh-provider branch
        // (`resolve_with_timeout`). With a synthetic handle whose provider id
        // isn't in the global config, it falls back to the shared provider
        // rather than panicking in `Config::get` — and still yields a working,
        // non-validated pipeline. Proves the override path is wired end-to-end.
        let tars = mock_handle();
        let (pipeline, _caps) = tars
            .pipeline_with_overrides(
                "mock",
                PipelineOverrides {
                    events: EventsOverride::Off,
                    timeout_secs: Some(1800),
                    ..Default::default()
                },
            )
            .expect("build pipeline with timeout override");
        // No validators ⇒ no validation layer; scope Off + EventsOverride::Off
        // ⇒ no event_emitter. Same canonical chain as `pipeline`.
        assert_eq!(
            pipeline.layer_names(),
            &["telemetry", "cache_lookup", "retry"],
            "timeout override must not alter the canonical chain shape",
        );
    }

    #[test]
    fn pipeline_wrapper_omits_validation_when_no_validators() {
        let tars = mock_handle();
        let pipeline = tars.pipeline("mock").expect("build default pipeline");
        assert!(
            !pipeline.layer_names().contains(&"validation"),
            "unexpected validation layer with no validators: {:?}",
            pipeline.layer_names(),
        );
    }

    // ── [resilience] config → pipeline ───────────────────────────────────

    #[test]
    fn no_resilience_config_yields_todays_chain() {
        // Default (both None) ⇒ the pipeline is byte-for-byte today's chain:
        // default retry, NO circuit-breaker wrapper. (Sink Off ⇒ no
        // event_emitter; no validators ⇒ no validation.)
        let tars = mock_handle();
        assert_eq!(tars.resilience, ResilienceConfig::default());
        let pipeline = tars.pipeline("mock").expect("build default pipeline");
        assert_eq!(
            pipeline.layer_names(),
            &["telemetry", "cache_lookup", "retry"],
            "absent [resilience] must reproduce today's canonical chain",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resilience_retry_count_flows_into_pipeline() {
        use futures::StreamExt;
        // A retry override of 4 MaybeRetriable attempts, no backoff, no
        // breaker: a single pipeline call against an always-`Internal`
        // (MaybeRetriable) provider must hit it exactly 4 times — proving the
        // [resilience.retry] config reached the pipeline's RetryConfig.
        let resilience = ResilienceConfig {
            retry: Some(tars_config::RetryTuning {
                max_attempts: Some(4),
                initial_backoff_secs: Some(0.0),
                max_backoff_secs: Some(0.0),
                multiplier: Some(1.0),
                respect_retry_after: Some(false),
                max_attempts_maybe_retriable: Some(4),
                max_wait_secs: Some(0.0),
                jitter_secs: Some(0.0),
            }),
            circuit_breaker: None,
        };
        let (tars, hits) =
            erroring_handle(resilience, || tars_types::ProviderError::Internal("boom".into()));
        let pipeline = Arc::new(tars.pipeline("mock").expect("build pipeline"));
        let res = pipeline
            .call(
                tars_types::ChatRequest::user(
                    tars_types::ModelHint::Explicit("m".into()),
                    "x",
                ),
                tars_types::RequestContext::test_default(),
            )
            .await;
        // Open-time error surfaces immediately (no stream); if a stream came
        // back, drain it.
        if let Ok(s) = res {
            let mut s = s;
            while s.next().await.is_some() {}
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "retry max_attempts_maybe_retriable=4 from [resilience] must drive 4 provider hits",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resilience_circuit_breaker_flows_into_pipeline() {
        // Breaker configured to open after 2 consecutive failures, retry
        // disabled (1 attempt) so each `call` = one provider hit. After two
        // failing calls the breaker opens and the 3rd call rejects with
        // CircuitOpen WITHOUT reaching the provider — proving
        // [resilience.circuit_breaker] reached PipelineOpts.circuit_breaker.
        let resilience = ResilienceConfig {
            retry: Some(tars_config::RetryTuning {
                max_attempts: Some(1),
                initial_backoff_secs: Some(0.0),
                max_backoff_secs: Some(0.0),
                multiplier: Some(1.0),
                respect_retry_after: Some(false),
                max_attempts_maybe_retriable: Some(1),
                max_wait_secs: Some(0.0),
                jitter_secs: Some(0.0),
            }),
            circuit_breaker: Some(tars_config::BreakerTuning {
                failure_threshold: Some(2),
                cooldown_secs: Some(30.0),
            }),
        };
        let (tars, hits) =
            erroring_handle(resilience, || tars_types::ProviderError::ModelOverloaded);
        let pipeline = Arc::new(tars.pipeline("mock").expect("build pipeline"));
        let req = || {
            tars_types::ChatRequest::user(tars_types::ModelHint::Explicit("m".into()), "x")
        };
        for _ in 0..2 {
            let e = pipeline
                .clone()
                .call(req(), tars_types::RequestContext::test_default())
                .await;
            assert!(matches!(
                e,
                Err(tars_types::ProviderError::ModelOverloaded)
            ));
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "both failures reached the provider",
        );
        let e = pipeline
            .clone()
            .call(req(), tars_types::RequestContext::test_default())
            .await;
        assert!(
            matches!(e, Err(tars_types::ProviderError::CircuitOpen { .. })),
            "breaker from [resilience] must reject the 3rd call as CircuitOpen",
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "open breaker spared the provider on the 3rd call",
        );
    }
}
