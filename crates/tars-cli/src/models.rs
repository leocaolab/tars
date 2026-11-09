//! Runs `tars models`: lists provider models from the model library in the tars home, or live
//! with `--live`; `update` rewrites the library and never edits the config or provider.toml.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tars_config::model_library::{
    Failure, FailureKind, LastQuery, Listing, LiveModel, ModelLibrary, NotAsked, ProviderEntry,
    diff_models, library_path,
};
use tars_config::model_spec::ModelSpec;
use tars_config::{Config, MODEL_KB, ModelCatalog, ProviderConfig, ProviderKind};
use tars_llm::EnvSource;
use tars_types::ProviderId;

use crate::Ending;
use crate::config::LoadConfigError;
use crate::home_env::HomeEnvError;
use crate::live_models::{ListOutcome, Plan, list_models, plan_for, with_body};
use crate::registry::{NotConfiguredError, configured};

mod render;

/// Bounds each provider's model-list query, so a dead local server or a hung TLS handshake cannot
/// stall the command.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A library older than this gets a "refresh it" note when listed.
const STALE_AFTER_DAYS: i64 = 7;

#[derive(Debug, clap::Args)]
pub(crate) struct ModelsArgs {
    #[command(subcommand)]
    update: Option<UpdateCommand>,

    #[command(flatten)]
    list: ListArgs,

    /// Emit a machine-readable JSON envelope instead of human text.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct ListArgs {
    /// Provider to list. Omit to list every configured provider.
    #[arg(value_name = "PROVIDER", value_parser = ProviderId::from_str)]
    provider: Option<ProviderId>,

    /// Bypass the persisted model library and query the provider APIs live.
    #[arg(long)]
    live: bool,
}

#[derive(Debug, clap::Subcommand)]
enum UpdateCommand {
    /// Refresh the model library from the live provider APIs (ids + the
    /// limits the API reports) and report what changed: models added and
    /// retired, limit changes, where `@latest` defaults now resolve, stale
    /// defaults, and where the shipped provider.toml is behind.
    Update {
        /// Provider to update. Omit to update every provider.
        #[arg(value_name = "PROVIDER", value_parser = ProviderId::from_str)]
        provider: Option<ProviderId>,
    },
}

/// What one `tars models` invocation asks for.
#[derive(Debug, PartialEq)]
pub(crate) enum ModelsCommand {
    List {
        provider: Option<ProviderId>,
        live: bool,
        json: bool,
    },
    Update {
        provider: Option<ProviderId>,
        json: bool,
    },
}

/// A listing flag given together with `update`, which clap parses but `update` has no use for.
#[derive(Debug, PartialEq, thiserror::Error)]
pub(crate) enum ModelsUsageError {
    #[error("`--live` lists models without writing the library, so it cannot go with `update`")]
    LiveWithUpdate,
    #[error("the provider goes after `update`: `tars models update {provider}`")]
    ProviderBeforeUpdate { provider: ProviderId },
}

impl ModelsArgs {
    /// Returns the command these arguments ask for.
    ///
    /// # Errors
    ///
    /// Refuses `--live` or a provider before `update`, rather than drop them.
    pub(crate) fn command(self) -> Result<ModelsCommand, ModelsUsageError> {
        let Self { update, list, json } = self;
        match update {
            None => Ok(ModelsCommand::List {
                provider: list.provider,
                live: list.live,
                json,
            }),
            Some(UpdateCommand::Update { provider }) => match (list.live, list.provider) {
                (true, _) => Err(ModelsUsageError::LiveWithUpdate),
                (false, Some(listed)) => {
                    Err(ModelsUsageError::ProviderBeforeUpdate { provider: listed })
                }
                (false, None) => Ok(ModelsCommand::Update { provider, json }),
            },
        }
    }
}

/// Why `tars models` did not list or refresh, or which providers a refresh failed.
#[derive(Debug, thiserror::Error)]
pub enum ModelsError {
    #[error(
        "cannot resolve tars home (set $TARS_HOME or ensure HOME is set) — \
         needed for the model library"
    )]
    NoHome,
    #[error(transparent)]
    Config(#[from] LoadConfigError),
    #[error(transparent)]
    Env(#[from] HomeEnvError),
    #[error(transparent)]
    NotConfigured(#[from] NotConfiguredError),
    #[error("reading model library at {}", path.display())]
    LibraryUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "model library is empty or missing ({}); run `tars models update` to build it, \
         or `tars models --live` to query the provider APIs directly",
        path.display()
    )]
    LibraryMissing { path: PathBuf },
    #[error("writing model library to {}", path.display())]
    LibraryUnwritable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("building HTTP client")]
    Client(#[source] reqwest::Error),
    #[error("writing the report as JSON")]
    Json(#[source] serde_json::Error),
    #[error("{} of {asked} providers did not refresh: {}", failed.len(), failed.join(", "))]
    RefreshFailed { failed: Vec<String>, asked: usize },
}

impl ModelsError {
    /// Says whether the command ran: a refresh that asked every provider ran, and the
    /// providers that did not answer with a list are what it found.
    pub(crate) fn ending(&self) -> Ending {
        match self {
            Self::RefreshFailed { .. } => Ending::Failed,
            Self::NoHome
            | Self::Config(_)
            | Self::Env(_)
            | Self::NotConfigured(_)
            | Self::LibraryUnreadable { .. }
            | Self::LibraryMissing { .. }
            | Self::LibraryUnwritable { .. }
            | Self::Client(_)
            | Self::Json(_) => Ending::NotRun,
        }
    }
}

/// Runs `tars models` against the CLI's config, with the model library in the tars home and
/// the keys resolved over the home's `.env`.
pub(crate) async fn execute(
    command: ModelsCommand,
    config_flag: Option<PathBuf>,
) -> Result<(), ModelsError> {
    let home = tars_config::resolve_home(None).ok_or(ModelsError::NoHome)?;
    let config = crate::config::load(config_flag)?;
    let env: Arc<dyn EnvSource> = Arc::new(crate::home_env::load()?);

    match command {
        ModelsCommand::Update { provider, json } => {
            run_update(&config, &home, provider.as_ref(), json, &env).await
        }
        ModelsCommand::List {
            provider,
            live,
            json,
        } => run_list(&config, &home, provider.as_ref(), live, json, &env).await,
    }
}

fn select_providers<'c>(
    config: &'c Config,
    only: Option<&ProviderId>,
) -> Result<Vec<(String, &'c ProviderConfig)>, NotConfiguredError> {
    let mut all: Vec<(String, &ProviderConfig)> = config
        .providers()
        .iter()
        .map(|(id, cfg)| (id.as_str().to_string(), cfg))
        .collect();
    all.sort_by(|a, b| a.0.cmp(&b.0));

    match only {
        None => Ok(all),
        Some(name) => {
            let id = configured(config, name.clone())?;
            Ok(all.into_iter().filter(|(n, _)| n == id.as_str()).collect())
        }
    }
}

fn build_client() -> Result<reqwest::Client, ModelsError> {
    crate::live_models::client(CONNECT_TIMEOUT).map_err(ModelsError::Client)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// How asking one provider for its list went: it was not asked (no list API), or the query
/// ended in an [`ListOutcome`].
enum Asked {
    Skipped { note: String },
    Queried(ListOutcome),
}

/// Asks the provider `cfg` declares for its list, if it has a list API.
async fn ask(cfg: &ProviderConfig, client: &reqwest::Client, env: &Arc<dyn EnvSource>) -> Asked {
    match plan_for(cfg) {
        Plan::Unlisted { note } => Asked::Skipped { note },
        Plan::Http(plan) => Asked::Queried(list_models(client, &plan, QUERY_TIMEOUT, env).await),
    }
}

/// Returns the row one query, run at `queried_at`, leaves for a provider with no previous row;
/// see [`ProviderEntry::following`] for one that has.
fn outcome_to_entry(
    name: &str,
    cfg: &ProviderConfig,
    asked: Asked,
    queried_at: &str,
) -> ProviderEntry {
    let failed = |kind, reason| LastQuery::Failed {
        failure: Failure {
            kind,
            reason,
            queried_at: queried_at.to_string(),
        },
        kept: None,
    };
    let not_asked = |why, note| LastQuery::NotAsked {
        why,
        note,
        kept: None,
    };
    let outcome = match asked {
        Asked::Skipped { note } => {
            return ProviderEntry::new(name, cfg, not_asked(NotAsked::NoListApi, note));
        }
        Asked::Queried(outcome) => outcome,
    };
    let last_query = match outcome {
        ListOutcome::Ok { models } => LastQuery::Listed(Listing {
            models,
            queried_at: queried_at.to_string(),
        }),
        ListOutcome::NoKey { var } => not_asked(NotAsked::NoKey, format!("no key: set ${var}")),
        ListOutcome::KeyUnresolved { reason } => failed(
            FailureKind::KeyUnresolved,
            format!("key did not resolve: {reason}"),
        ),
        ListOutcome::KeyInvalid { header } => failed(
            FailureKind::KeyInvalid,
            format!("key is not a valid `{header}` header value (a newline in it?) — fix the key"),
        ),
        ListOutcome::AuthFailed { status, body } => failed(
            FailureKind::AuthFailed,
            with_body(
                format!("auth rejected (HTTP {status}) — key invalid/expired?"),
                &body,
            ),
        ),
        ListOutcome::HttpStatus { status, body } => failed(
            FailureKind::HttpError,
            with_body(format!("HTTP {status}"), &body),
        ),
        ListOutcome::Unreachable { detail } => failed(FailureKind::Unreachable, detail),
        ListOutcome::NotRunning { detail } => not_asked(
            NotAsked::ServerNotRunning,
            format!("server not running: {detail}"),
        ),
        ListOutcome::ParseError { detail } => failed(FailureKind::ParseError, detail),
    };
    ProviderEntry::new(name, cfg, last_query)
}

// ───────────────────────────── report ─────────────────────────────

/// Which limit a [`LimitChange`] / [`Finding::ShippedLimitDiffers`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenLimit {
    Context,
    MaxOutput,
}

impl TokenLimit {
    fn of_live(self, m: &LiveModel) -> Option<u64> {
        match self {
            Self::Context => m.context,
            Self::MaxOutput => m.max_output,
        }
    }

    fn of_shipped(self, m: &tars_config::ModelEntry) -> Option<u64> {
        match self {
            Self::Context => m.context,
            Self::MaxOutput => m.max_output,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::MaxOutput => "max output",
        }
    }
}

/// A limit the API reported differently than at the previous refresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LimitChange {
    pub model: String,
    pub field: TokenLimit,
    pub old: Option<u64>,
    pub new: Option<u64>,
}

/// One refreshed provider: its new row and the diff against the last refresh.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderRefresh {
    pub name: String,
    pub entry: ProviderEntry,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub limit_changes: Vec<LimitChange>,
}

/// Something a refresh means for the config or for tars's shipped model data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Finding {
    /// A `<series>@latest` default resolves to `model`; `previous` is where
    /// it resolved before this refresh, when that differs.
    DefaultResolved {
        provider: String,
        spec: String,
        model: String,
        previous: Option<String>,
    },
    /// A default spec that does not resolve against the refreshed library, or does not parse.
    DefaultUnresolvable {
        provider: String,
        spec: String,
        error: String,
    },
    /// A concrete default the provider's API does not list.
    DefaultNotListed { provider: String, model: String },
    /// A concrete default in a series that has a newer model.
    NewerInSeries {
        provider: String,
        model: String,
        series: String,
        latest: String,
    },
    /// The newest model of a series has no row in provider.toml: tars knows
    /// its limits from the API, but not its price or thinking mode.
    ShippedMissingModel { catalog: String, model: String },
    /// A provider.toml row the API does not list (by id or alias).
    ShippedModelNotListed { catalog: String, model: String },
    /// provider.toml states a limit the API contradicts. The live number is
    /// the one the runtime uses.
    ShippedLimitDiffers {
        catalog: String,
        model: String,
        field: TokenLimit,
        shipped: u64,
        live: u64,
    },
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DefaultResolved {
                provider,
                spec,
                model,
                previous: Some(prev),
            } => write!(
                f,
                "{provider}: `{spec}` now resolves to {model} (was {prev})"
            ),
            Self::DefaultResolved {
                provider,
                spec,
                model,
                previous: None,
            } => write!(f, "{provider}: `{spec}` resolves to {model}"),
            Self::DefaultUnresolvable {
                provider,
                spec,
                error,
            } => write!(f, "{provider}: default `{spec}` does not resolve — {error}"),
            Self::DefaultNotListed { provider, model } => write!(
                f,
                "{provider}: default_model `{model}` is not in the API's model list \
                 (retired or renamed?) — fix the config"
            ),
            Self::NewerInSeries {
                provider,
                model,
                series,
                latest,
            } => write!(
                f,
                "{provider}: default_model `{model}` is behind — the newest {series} is \
                 {latest}; write `{series}@latest` to follow it automatically"
            ),
            Self::ShippedMissingModel { catalog, model } => write!(
                f,
                "provider.toml [{catalog}]: no row for {model} — its limits come from the API, \
                 but its price and thinking mode are unknown to tars until a row is added"
            ),
            Self::ShippedModelNotListed { catalog, model } => write!(
                f,
                "provider.toml [{catalog}]: row {model} is no longer in the API's model list"
            ),
            Self::ShippedLimitDiffers {
                catalog,
                model,
                field,
                shipped,
                live,
            } => write!(
                f,
                "provider.toml [{catalog}]: {model} {} is {shipped}, the API reports {live} \
                 (the runtime uses {live})",
                field.label()
            ),
        }
    }
}

/// Diffs one provider's fresh row against its previous row.
fn provider_refresh(
    name: &str,
    prev: Option<&ProviderEntry>,
    entry: ProviderEntry,
) -> ProviderRefresh {
    let prev_listing = prev.and_then(|p| p.last_query.listing());
    let (added, removed, limit_changes) = match (prev_listing, &entry.last_query) {
        (Some(p), LastQuery::Listed(listing)) => {
            let (added, removed) = diff_models(p.ids(), listing.ids());
            let mut changes = Vec::new();
            for new in &listing.models {
                let Some(old) = p.model(&new.id) else {
                    continue;
                };
                for field in [TokenLimit::Context, TokenLimit::MaxOutput] {
                    let (o, n) = (field.of_live(old), field.of_live(new));
                    if o != n {
                        changes.push(LimitChange {
                            model: new.id.clone(),
                            field,
                            old: o,
                            new: n,
                        });
                    }
                }
            }
            (added, removed, changes)
        }
        (None, LastQuery::Listed(listing)) => {
            (listing.ids().map(str::to_string).collect(), vec![], vec![])
        }
        (_, LastQuery::NotAsked { .. } | LastQuery::Failed { .. }) => (vec![], vec![], vec![]),
    };
    ProviderRefresh {
        name: name.to_string(),
        entry,
        added,
        removed,
        limit_changes,
    }
}

/// Lists what the refreshed library means for each configured provider's default and for the
/// shipped provider data.
fn findings(
    providers: &[(String, &ProviderConfig)],
    old: Option<&ModelLibrary>,
    new: &ModelLibrary,
) -> Vec<Finding> {
    let kb = &*MODEL_KB;
    let before = ModelCatalog::new(kb, old.cloned());
    let after = ModelCatalog::new(kb, Some(new.clone()));
    let mut out = Vec::new();

    for (name, cfg) in providers {
        let catalog = cfg.catalog_name(name);
        let spec = cfg.default_model();
        match ModelSpec::parse(spec) {
            Ok(ModelSpec::Latest { .. }) => match after.resolve(catalog, spec) {
                Ok(r) => {
                    let previous = before
                        .resolve(catalog, spec)
                        .ok()
                        .map(|p| p.id)
                        .filter(|p| *p != r.id);
                    out.push(Finding::DefaultResolved {
                        provider: name.clone(),
                        spec: spec.to_string(),
                        model: r.id,
                        previous,
                    });
                }
                Err(e) => out.push(Finding::DefaultUnresolvable {
                    provider: name.clone(),
                    spec: spec.to_string(),
                    error: e.to_string(),
                }),
            },
            Ok(ModelSpec::Concrete(model)) => {
                let Some(listed) = after.live_list(cfg.listing_catalog(name)) else {
                    continue;
                };
                if listed.model(model).is_none() {
                    out.push(Finding::DefaultNotListed {
                        provider: name.clone(),
                        model: model.to_string(),
                    });
                }
                let Some(def) = kb.providers.get(catalog) else {
                    continue;
                };
                for (series, pattern) in &def.series {
                    let Some(own) = pattern.rank(model) else {
                        continue;
                    };
                    if let Some(latest) = pattern.latest(listed.ids())
                        && pattern.rank(latest).is_some_and(|r| r > own)
                    {
                        out.push(Finding::NewerInSeries {
                            provider: name.clone(),
                            model: model.to_string(),
                            series: series.clone(),
                            latest: latest.to_string(),
                        });
                    }
                }
            }
            Err(e) => out.push(Finding::DefaultUnresolvable {
                provider: name.clone(),
                spec: spec.to_string(),
                error: e.to_string(),
            }),
        }
    }

    // Shipped-data drift, once per refreshed catalog block.
    for (catalog, listed) in new.freshest_by_catalog() {
        let Some(def) = kb.providers.get(catalog) else {
            continue;
        };
        for pattern in def.series.values() {
            if let Some(latest) = pattern.latest(listed.ids())
                && def.find_model(latest).is_none()
            {
                out.push(Finding::ShippedMissingModel {
                    catalog: catalog.to_string(),
                    model: latest.to_string(),
                });
            }
        }
        for row in &def.models {
            let live = std::iter::once(&row.id)
                .chain(&row.aliases)
                .find_map(|id| listed.model(id));
            let Some(live) = live else {
                out.push(Finding::ShippedModelNotListed {
                    catalog: catalog.to_string(),
                    model: row.id.clone(),
                });
                continue;
            };
            for field in [TokenLimit::Context, TokenLimit::MaxOutput] {
                if let (Some(shipped), Some(reported)) =
                    (field.of_shipped(row), field.of_live(live))
                    && shipped != reported
                {
                    out.push(Finding::ShippedLimitDiffers {
                        catalog: catalog.to_string(),
                        model: row.id.clone(),
                        field,
                        shipped,
                        live: reported,
                    });
                }
            }
        }
    }
    out
}

// ───────────────────────────── update ─────────────────────────────

async fn run_update(
    config: &Config,
    home: &Path,
    only: Option<&ProviderId>,
    json: bool,
    env: &Arc<dyn EnvSource>,
) -> Result<(), ModelsError> {
    let providers = select_providers(config, only)?;
    let path = library_path(home);
    let old = ModelLibrary::load(&path).map_err(|source| ModelsError::LibraryUnreadable {
        path: path.clone(),
        source,
    })?;
    let client = build_client()?;

    let prev_of = |name: &str| old.as_ref().and_then(|l| l.providers.get(name));
    let mut fresh: BTreeMap<String, ProviderEntry> = BTreeMap::new();
    for (name, cfg) in &providers {
        let asked = ask(cfg, &client, env).await;
        let entry = outcome_to_entry(name, cfg, asked, &now_rfc3339()).following(prev_of(name));
        fresh.insert(name.clone(), entry);
    }

    // Merge into the prior library: a single-provider update must not drop the
    // rows for providers it didn't touch.
    let mut lib = old
        .clone()
        .unwrap_or_else(|| ModelLibrary::new(now_rfc3339()));
    lib.updated_at = now_rfc3339();
    for (name, entry) in &fresh {
        lib.providers.insert(name.clone(), entry.clone());
    }
    lib.save(&path)
        .map_err(|source| ModelsError::LibraryUnwritable {
            path: path.clone(),
            source,
        })?;

    let changes: Vec<ProviderRefresh> = fresh
        .into_iter()
        .map(|(name, entry)| provider_refresh(&name, prev_of(&name), entry))
        .collect();
    let findings = findings(&providers, old.as_ref(), &lib);

    if json {
        let env = serde_json::json!({
            "command": "models update",
            "library": path.display().to_string(),
            "providers": changes,
            "findings": findings.iter().map(|f| serde_json::json!({
                "finding": f,
                "message": f.to_string(),
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&env).map_err(ModelsError::Json)?
        );
    } else {
        render::print_update_human(&path, &changes, &findings);
    }

    let failed: Vec<String> = changes
        .iter()
        .filter(|c| match &c.entry.last_query {
            LastQuery::Failed { .. } => true,
            LastQuery::Listed(_) | LastQuery::NotAsked { .. } => false,
        })
        .map(|c| c.name.clone())
        .collect();
    if !failed.is_empty() {
        return Err(ModelsError::RefreshFailed {
            failed,
            asked: changes.len(),
        });
    }
    Ok(())
}

// ───────────────────────────── list ─────────────────────────────

async fn run_list(
    config: &Config,
    home: &Path,
    only: Option<&ProviderId>,
    live: bool,
    json: bool,
    env: &Arc<dyn EnvSource>,
) -> Result<(), ModelsError> {
    let providers = select_providers(config, only)?;

    let lib = if live {
        let client = build_client()?;
        let mut lib = ModelLibrary::new(now_rfc3339());
        for (name, cfg) in &providers {
            let asked = ask(cfg, &client, env).await;
            lib.providers.insert(
                name.clone(),
                outcome_to_entry(name, cfg, asked, &now_rfc3339()),
            );
        }
        lib
    } else {
        let path = library_path(home);
        let lib = ModelLibrary::load(&path).map_err(|source| ModelsError::LibraryUnreadable {
            path: path.clone(),
            source,
        })?;
        lib.filter(|l| !l.providers.is_empty())
            .ok_or(ModelsError::LibraryMissing { path })?
    };

    let catalog = ModelCatalog::new(&MODEL_KB, Some(lib.clone()));
    // Rows carry the configured default as it is now, which may have changed since the update.
    let rows: Vec<ListingRow> = providers
        .iter()
        .map(|(name, cfg)| {
            let entry = lib.providers.get(name).cloned();
            let spec = cfg.default_model().to_string();
            let resolved = catalog
                .resolve(cfg.catalog_name(name), &spec)
                .map(|r| r.id)
                .map_err(|e| e.to_string());
            let listing = entry.as_ref().and_then(|e| e.last_query.listing());
            let default_listed = match (&resolved, listing) {
                (Ok(id), Some(listing)) => Some(listing.model(id).is_some()),
                _ => None,
            };
            ListingRow {
                name: name.clone(),
                provider_type: cfg.kind(),
                catalog: cfg.listing_catalog(name).to_string(),
                entry,
                default_spec: spec,
                default_resolved: resolved,
                default_listed,
            }
        })
        .collect();

    let age_days = (!live).then(|| age_days(&lib.updated_at, chrono::Utc::now()));

    if json {
        render::print_list_json(live, &lib.updated_at, &rows).map_err(ModelsError::Json)?;
    } else {
        render::print_list_human(live, &lib.updated_at, age_days, &rows);
    }
    Ok(())
}

/// Returns how many whole days before `now` the library was written at `updated_at`.
fn age_days(
    updated_at: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<i64, chrono::ParseError> {
    chrono::DateTime::parse_from_rfc3339(updated_at)
        .map(|t| (now - t.with_timezone(&chrono::Utc)).num_days())
}

struct ListingRow {
    name: String,
    /// The type and catalog as the config states them now.
    provider_type: ProviderKind,
    catalog: String,
    /// The provider's library row; `None` when no update has listed it yet.
    entry: Option<ProviderEntry>,
    default_spec: String,
    /// The default resolved against the listed catalog, or why it didn't.
    default_resolved: Result<String, String>,
    /// Whether the resolved default is in the listed models — `None` when
    /// there's no list to check against.
    default_listed: Option<bool>,
}

impl ListingRow {
    fn default_label(&self) -> String {
        match &self.default_resolved {
            Ok(id) if *id == self.default_spec => id.clone(),
            Ok(id) => format!("{} → {id}", self.default_spec),
            Err(e) => format!("{} (does not resolve: {e})", self.default_spec),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::{Auth, HttpOverrides};

    fn gemini_cfg(default: &str) -> ProviderConfig {
        ProviderConfig::Gemini {
            base_url: None,
            auth: Auth::env("GEMINI_API_KEY"),
            default_model: default.into(),
            extras: HttpOverrides::default(),
        }
    }

    fn live(id: &str, max_output: Option<u64>) -> LiveModel {
        LiveModel {
            id: id.into(),
            context: None,
            max_output,
        }
    }

    fn gemini_row(models: Vec<LiveModel>, at: &str) -> ProviderEntry {
        ProviderEntry {
            provider_type: ProviderKind::Gemini,
            catalog: "gemini".into(),
            default_model: "flash@latest".into(),
            last_query: LastQuery::Listed(Listing {
                models,
                queried_at: at.into(),
            }),
        }
    }

    /// Returns every id provider.toml's gemini block has, at the shipped limits, plus
    /// `extra` — a list with no shipped drift of its own.
    fn full_gemini_list(extra: &[&str]) -> Vec<LiveModel> {
        let mut v: Vec<LiveModel> = MODEL_KB.providers["gemini"]
            .models
            .iter()
            .map(|m| LiveModel {
                id: m.id.clone(),
                context: m.context,
                max_output: m.max_output,
            })
            .collect();
        v.extend(extra.iter().map(|id| LiveModel::id_only(*id)));
        v
    }

    fn lib_with(row: ProviderEntry) -> ModelLibrary {
        let mut lib = ModelLibrary::new(
            row.last_query
                .queried_at()
                .expect("a listed row has a query time")
                .to_string(),
        );
        lib.providers.insert("gemini_flash".into(), row);
        lib
    }

    #[test]
    fn outcome_nokey_carries_var_name_not_a_sentinel() {
        let cfg = gemini_cfg("gemini-2.5-flash");
        let e = outcome_to_entry(
            "gemini_flash",
            &cfg,
            Asked::Queried(ListOutcome::NoKey {
                var: "GEMINI_API_KEY".into(),
            }),
            "2026-11-06T00:00:00Z",
        );
        let LastQuery::NotAsked { why, note, .. } = &e.last_query else {
            panic!("a missing key lists nothing: {e:?}");
        };
        assert_eq!(*why, NotAsked::NoKey);
        assert_eq!(e.catalog, "gemini");
        assert_eq!(note, "no key: set $GEMINI_API_KEY");
    }

    #[test]
    fn change_reports_added_removed_and_limit_changes() {
        let old = gemini_row(
            vec![live("a", Some(8192)), live("gone", None)],
            "2026-09-01T00:00:00Z",
        );
        let new = gemini_row(
            vec![live("a", Some(65_536)), live("b", None)],
            "2026-09-14T00:00:00Z",
        );
        let c = provider_refresh("gemini_flash", Some(&old), new);
        assert_eq!(c.added, vec!["b"]);
        assert_eq!(c.removed, vec!["gone"]);
        assert_eq!(
            c.limit_changes,
            vec![LimitChange {
                model: "a".into(),
                field: TokenLimit::MaxOutput,
                old: Some(8192),
                new: Some(65_536),
            }]
        );
    }

    #[test]
    fn a_latest_default_reports_where_it_moved() {
        let cfg = gemini_cfg("flash@latest");
        let providers = vec![("gemini_flash".to_string(), &cfg)];
        let old = lib_with(gemini_row(full_gemini_list(&[]), "2026-09-01T00:00:00Z"));
        let new = lib_with(gemini_row(
            full_gemini_list(&["gemini-3.9-flash"]),
            "2026-09-14T00:00:00Z",
        ));
        let got = findings(&providers, Some(&old), &new);
        assert!(got.contains(&Finding::DefaultResolved {
            provider: "gemini_flash".into(),
            spec: "flash@latest".into(),
            model: "gemini-3.9-flash".into(),
            previous: Some("gemini-3.8-flash".into()),
        }));
        // provider.toml has no row for the new model — say so.
        assert!(got.contains(&Finding::ShippedMissingModel {
            catalog: "gemini".into(),
            model: "gemini-3.9-flash".into(),
        }));
    }

    #[test]
    fn a_failed_refresh_leaves_latest_on_the_last_listed_model() {
        let cfg = gemini_cfg("flash@latest");
        let providers = vec![("gemini_flash".to_string(), &cfg)];
        let old = lib_with(gemini_row(
            full_gemini_list(&["gemini-3.9-flash"]),
            "2026-09-01T00:00:00Z",
        ));
        let refused = outcome_to_entry(
            "gemini_flash",
            &cfg,
            Asked::Queried(ListOutcome::Unreachable {
                detail: "tcp connect error: Connection refused (os error 61)".into(),
            }),
            "2026-09-14T00:00:00Z",
        )
        .following(old.providers.get("gemini_flash"));
        let change = provider_refresh("gemini_flash", old.providers.get("gemini_flash"), refused);
        assert!(change.removed.is_empty() && change.added.is_empty());

        let new = lib_with(change.entry);
        let got = findings(&providers, Some(&old), &new);
        assert!(got.contains(&Finding::DefaultResolved {
            provider: "gemini_flash".into(),
            spec: "flash@latest".into(),
            model: "gemini-3.9-flash".into(),
            previous: None,
        }));
    }

    #[test]
    fn a_concrete_default_behind_its_series_is_flagged() {
        let cfg = gemini_cfg("gemini-2.5-flash");
        let providers = vec![("gemini_flash".to_string(), &cfg)];
        let new = lib_with(gemini_row(full_gemini_list(&[]), "2026-09-14T00:00:00Z"));
        let got = findings(&providers, None, &new);
        assert_eq!(
            got,
            vec![Finding::NewerInSeries {
                provider: "gemini_flash".into(),
                model: "gemini-2.5-flash".into(),
                series: "flash".into(),
                latest: "gemini-3.8-flash".into(),
            }]
        );
        assert!(got[0].to_string().contains("write `flash@latest`"));
    }

    #[test]
    fn shipped_drift_retired_row_and_contradicted_limit() {
        let cfg = gemini_cfg("flash@latest");
        let providers = vec![("gemini_flash".to_string(), &cfg)];
        let mut models: Vec<LiveModel> = full_gemini_list(&[])
            .into_iter()
            .filter(|m| m.id != "gemini-2.5-flash-lite")
            .collect();
        for m in &mut models {
            if m.id == "gemini-3.8-flash" {
                m.max_output = Some(131_072);
            }
        }
        let new = lib_with(gemini_row(models, "2026-09-14T00:00:00Z"));
        let got = findings(&providers, None, &new);
        assert!(got.contains(&Finding::ShippedModelNotListed {
            catalog: "gemini".into(),
            model: "gemini-2.5-flash-lite".into(),
        }));
        assert!(got.contains(&Finding::ShippedLimitDiffers {
            catalog: "gemini".into(),
            model: "gemini-3.8-flash".into(),
            field: TokenLimit::MaxOutput,
            shipped: 65_536,
            live: 131_072,
        }));
    }

    #[test]
    fn select_providers_errors_on_unknown_name() {
        let toml = r#"
            [providers.gemini_flash]
            type = "gemini"
            default_model = "flash@latest"
            auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
        "#;
        let cfg = Config::from_toml(toml).expect("config");
        let err = select_providers(&cfg, Some(&ProviderId::new("nope"))).unwrap_err();
        assert!(format!("{err}").contains("not configured"));
    }

    #[test]
    fn select_providers_filters_to_one() {
        let toml = r#"
            [providers.gemini_flash]
            type = "gemini"
            default_model = "flash@latest"
            auth = { kind = "secret", secret = { source = "env", var = "GEMINI_API_KEY" } }
        "#;
        let cfg = Config::from_toml(toml).expect("config");
        let sel = select_providers(&cfg, Some(&ProviderId::new("gemini_flash"))).unwrap();
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].0, "gemini_flash");
    }

    #[test]
    fn an_unset_key_variable_no_list_api_and_a_stopped_local_server_are_not_asked_not_failed() {
        let cfg = gemini_cfg("gemini-2.5-flash");
        let at = "2026-11-06T00:00:00Z";
        let ended = |asked| outcome_to_entry("g", &cfg, asked, at).last_query;
        let detail = || "refused".to_string();
        for asked in [
            Asked::Queried(ListOutcome::NoKey { var: "K".into() }),
            Asked::Skipped { note: "cli".into() },
            Asked::Queried(ListOutcome::NotRunning { detail: detail() }),
        ] {
            let query = ended(asked);
            assert!(matches!(query, LastQuery::NotAsked { .. }), "{query:?}");
        }
        for asked in [
            Asked::Queried(ListOutcome::KeyUnresolved { reason: detail() }),
            Asked::Queried(ListOutcome::KeyInvalid {
                header: "x-goog-api-key",
            }),
            Asked::Queried(ListOutcome::AuthFailed {
                status: 401,
                body: crate::live_models::Body::Read(detail()),
            }),
            Asked::Queried(ListOutcome::HttpStatus {
                status: 500,
                body: crate::live_models::Body::Read(detail()),
            }),
            Asked::Queried(ListOutcome::Unreachable { detail: detail() }),
            Asked::Queried(ListOutcome::ParseError { detail: detail() }),
        ] {
            let query = ended(asked);
            assert!(matches!(query, LastQuery::Failed { .. }), "{query:?}");
        }
    }

    #[test]
    fn a_refusal_row_keeps_the_servers_body_and_an_unreadable_key_file_its_reason() {
        let cfg = gemini_cfg("gemini-2.5-flash");
        let at = "2026-11-06T00:00:00Z";
        let reason_of = |outcome| {
            let e = outcome_to_entry("g", &cfg, Asked::Queried(outcome), at);
            match e.last_query {
                LastQuery::Failed { failure, .. } => {
                    (failure.kind, failure.reason, failure.queried_at)
                }
                LastQuery::Listed(_) | LastQuery::NotAsked { .. } => {
                    panic!("a refusal was sent and lists nothing")
                }
            }
        };
        let (kind, reason, queried_at) = reason_of(ListOutcome::HttpStatus {
            status: 429,
            body: crate::live_models::Body::Read("quota exceeded".into()),
        });
        assert_eq!(
            (kind, reason.as_str(), queried_at.as_str()),
            (FailureKind::HttpError, "HTTP 429: quota exceeded", at)
        );
        let (kind, reason, _) = reason_of(ListOutcome::KeyUnresolved {
            reason: "io: opening /k: denied".into(),
        });
        assert_eq!(kind, FailureKind::KeyUnresolved);
        assert!(reason.contains("/k: denied"), "{reason}");
    }

    #[test]
    fn a_library_age_is_whole_days_and_a_bad_timestamp_is_an_error() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-08T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(age_days("2026-11-01T12:00:00Z", now).unwrap(), 7);
        assert_eq!(age_days("2026-11-01T12:00:01Z", now).unwrap(), 6);
        assert!(age_days("yesterday", now).is_err());
    }
}
