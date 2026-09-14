//! `tars models` — discover provider models over a persisted **model library**.
//!
//! `tars models update` refreshes `$TARS_HOME/models.json` — the live layer of
//! the model catalog the runtime reads (see `tars_config::model_catalog`) —
//! and reports what the refresh means: models added and retired, limits that
//! changed, where each `<series>@latest` default now points, configured
//! defaults a newer model in their series has overtaken, and places the
//! shipped `data/provider.toml` has fallen behind the provider's API.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use tars_config::model_library::{
    EntryStatus, LiveModel, ModelLibrary, ProviderEntry, diff_models, library_path,
};
use tars_config::model_spec::{ModelSpec, SeriesPattern};
use tars_config::{Config, ConfigManager, MODEL_KB, ModelCatalog, ProviderConfig};
use tars_types::ProviderId;

use crate::cli::model_query::{Outcome, plan_for, query};

/// Per-request budget for a live model-list query. Bounds each provider so a
/// dead local server or a hung TLS handshake can't stall the command.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A library older than this gets a "refresh it" note when listed.
const STALE_AFTER_DAYS: i64 = 7;

#[derive(Debug, clap::Args)]
pub struct ModelsArgs {
    #[command(subcommand)]
    action: Option<ModelsAction>,

    /// (query mode) Provider name to show. Omit to show every configured
    /// provider. Ignored when a subcommand is given.
    #[arg(value_name = "PROVIDER")]
    provider: Option<String>,

    /// Bypass the persisted model library and query the provider APIs live.
    #[arg(long)]
    live: bool,

    /// Emit a machine-readable JSON envelope instead of human text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Subcommand)]
enum ModelsAction {
    /// Refresh the model library from the live provider APIs (ids + the
    /// limits the API reports) and report what changed: models added and
    /// retired, limit changes, where `@latest` defaults now resolve, stale
    /// defaults, and where the shipped provider.toml is behind.
    Update {
        /// Provider to update. Omit to update every provider.
        #[arg(value_name = "PROVIDER")]
        provider: Option<String>,

        /// Emit a machine-readable JSON envelope instead of human text.
        #[arg(long)]
        json: bool,
    },
}

/// Load config + `.env` from the tars home, then dispatch query/update.
pub async fn execute(args: ModelsArgs, config_flag: Option<PathBuf>) -> Result<()> {
    let home = tars_config::resolve_home(None).context(
        "cannot resolve tars home (set $TARS_HOME or ensure HOME is set) — \
         needed for the model library and .env",
    )?;

    // Best-effort: load `$TARS_HOME/.env` so env-var provider auth resolves
    // without the user pre-exporting keys. Never overrides an already-set var
    // (shell env wins) and never fails the command if the file is absent.
    let _ = dotenvy::from_path(home.join(".env"));

    let config_path = config_flag.unwrap_or_else(|| home.join("config.toml"));
    let config = ConfigManager::load_from_file(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    match args.action {
        Some(ModelsAction::Update { provider, json }) => {
            run_update(&config, &home, provider.as_deref(), json).await
        }
        None => {
            run_query(
                &config,
                &home,
                args.provider.as_deref(),
                args.live,
                args.json,
            )
            .await
        }
    }
}

fn select_providers<'c>(
    config: &'c Config,
    only: Option<&str>,
) -> Result<Vec<(String, &'c ProviderConfig)>> {
    let mut all: Vec<(String, &ProviderConfig)> = config
        .providers
        .iter()
        .map(|(id, cfg)| (id.as_str().to_string(), cfg))
        .collect();
    all.sort_by(|a, b| a.0.cmp(&b.0));

    match only {
        None => Ok(all),
        Some(name) => {
            let cfg = config
                .providers
                .get(&ProviderId::new(name))
                .with_context(|| {
                    let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
                    format!(
                        "provider '{name}' is not configured. Known providers: {}",
                        names.join(", ")
                    )
                })?;
            Ok(vec![(name.to_string(), cfg)])
        }
    }
}

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .context("building HTTP client")
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn outcome_to_entry(name: &str, cfg: &ProviderConfig, outcome: Outcome) -> ProviderEntry {
    let base = |status, models, note| ProviderEntry {
        provider_type: provider_type_of(cfg).to_string(),
        catalog: cfg.catalog_name(name).to_string(),
        default_model: cfg.default_model().to_string(),
        status,
        models,
        note,
        queried_at: now_rfc3339(),
    };
    match outcome {
        Outcome::Ok { models } => base(EntryStatus::Ok, models, None),
        Outcome::NoKey { var } => base(
            EntryStatus::NoKey,
            vec![],
            Some(format!("no key: set ${var}")),
        ),
        Outcome::Skipped { note } => base(EntryStatus::Skipped, vec![], Some(note)),
        Outcome::AuthFailed { status } => base(
            EntryStatus::AuthFailed,
            vec![],
            Some(format!(
                "auth rejected (HTTP {status}) — key invalid/expired?"
            )),
        ),
        Outcome::HttpStatus { status } => base(
            EntryStatus::HttpError,
            vec![],
            Some(format!("HTTP {status}")),
        ),
        Outcome::Unreachable { detail } => base(EntryStatus::Unreachable, vec![], Some(detail)),
        Outcome::ParseError { detail } => base(EntryStatus::ParseError, vec![], Some(detail)),
    }
}

// ───────────────────────────── report ─────────────────────────────

/// Which limit a [`LimitChange`] / [`Finding::ShippedLimitDiffers`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitField {
    Context,
    MaxOutput,
}

impl LimitField {
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
    pub field: LimitField,
    pub old: Option<u64>,
    pub new: Option<u64>,
}

/// One refreshed provider: its new row and the diff against the last refresh.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderChange {
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
    /// A default spec no longer resolves at all.
    DefaultUnresolvable {
        provider: String,
        spec: String,
        error: String,
    },
    /// A concrete default the provider's API no longer lists.
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
    /// A provider.toml row the API no longer lists (by id or alias).
    ShippedModelNotListed { catalog: String, model: String },
    /// provider.toml states a limit the API contradicts. The live number is
    /// the one the runtime uses.
    ShippedLimitDiffers {
        catalog: String,
        model: String,
        field: LimitField,
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

/// Diff one provider's fresh row against its previous row.
fn provider_change(
    name: &str,
    prev: Option<&ProviderEntry>,
    entry: ProviderEntry,
) -> ProviderChange {
    let prev_ok = prev.filter(|p| p.status == EntryStatus::Ok);
    let (added, removed, limit_changes) = match (prev_ok, entry.status) {
        (Some(p), EntryStatus::Ok) => {
            let (added, removed) = diff_models(p.ids(), entry.ids());
            let mut changes = Vec::new();
            for new in &entry.models {
                let Some(old) = p.model(&new.id) else {
                    continue;
                };
                for field in [LimitField::Context, LimitField::MaxOutput] {
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
        // No prior Ok row → everything currently listed is "added".
        (_, EntryStatus::Ok) => (entry.ids().map(str::to_string).collect(), vec![], vec![]),
        _ => (vec![], vec![], vec![]),
    };
    ProviderChange {
        name: name.to_string(),
        entry,
        added,
        removed,
        limit_changes,
    }
}

/// What the refreshed library means for each configured provider's default
/// and for the shipped provider data. Pure over the two libraries.
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
                let Some(listed) = after.live_list(catalog) else {
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
                for (series, raw) in &def.series {
                    let Some(pattern) = SeriesPattern::parse(raw) else {
                        continue;
                    };
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
        for raw in def.series.values() {
            let Some(pattern) = SeriesPattern::parse(raw) else {
                continue;
            };
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
            for field in [LimitField::Context, LimitField::MaxOutput] {
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

async fn run_update(config: &Config, home: &Path, only: Option<&str>, json: bool) -> Result<()> {
    let providers = select_providers(config, only)?;
    let path = library_path(home);
    let old = ModelLibrary::load(&path)
        .with_context(|| format!("reading existing model library at {}", path.display()))?;
    let client = build_client()?;

    // Query each selected provider live.
    let mut fresh: BTreeMap<String, ProviderEntry> = BTreeMap::new();
    for (name, cfg) in &providers {
        let outcome = query(&client, &plan_for(cfg), QUERY_TIMEOUT).await;
        fresh.insert(name.clone(), outcome_to_entry(name, cfg, outcome));
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
        .with_context(|| format!("writing model library to {}", path.display()))?;

    let changes: Vec<ProviderChange> = fresh
        .into_iter()
        .map(|(name, entry)| {
            let prev = old.as_ref().and_then(|l| l.providers.get(&name));
            provider_change(&name, prev, entry)
        })
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
        println!("{}", serde_json::to_string_pretty(&env)?);
    } else {
        print_update_human(&path, &changes, &findings);
    }
    Ok(())
}

fn print_update_human(path: &Path, changes: &[ProviderChange], findings: &[Finding]) {
    println!("Updated model library: {}\n", path.display());

    for c in changes {
        match c.entry.status {
            EntryStatus::Ok => {
                println!(
                    "  {}  ({}) — {} models",
                    c.name,
                    c.entry.provider_type,
                    c.entry.models.len()
                );
                if !c.added.is_empty() {
                    println!("      + added:   {}", c.added.join(", "));
                }
                if !c.removed.is_empty() {
                    println!(
                        "      - removed: {}  (deprecated/retired)",
                        c.removed.join(", ")
                    );
                }
                for l in &c.limit_changes {
                    println!(
                        "      ~ {} {}: {} → {}",
                        l.model,
                        l.field.label(),
                        fmt_opt(l.old),
                        fmt_opt(l.new)
                    );
                }
                if c.added.is_empty() && c.removed.is_empty() && c.limit_changes.is_empty() {
                    println!("      (no change)");
                }
            }
            _ => {
                let note = c.entry.note.as_deref().unwrap_or("not queried");
                println!(
                    "  {}  ({}) — skipped: {}",
                    c.name, c.entry.provider_type, note
                );
            }
        }
    }

    let (resolved, notes): (Vec<&Finding>, Vec<&Finding>) = findings
        .iter()
        .partition(|f| matches!(f, Finding::DefaultResolved { previous: None, .. }));
    if !resolved.is_empty() {
        println!("\nDefaults:");
        for f in resolved {
            println!("  {f}");
        }
    }
    if !notes.is_empty() {
        println!("\n⚠ needs attention (config and provider.toml are not auto-edited):");
        for f in notes {
            println!("  {f}");
        }
    }
}

fn fmt_opt(n: Option<u64>) -> String {
    n.map_or_else(|| "not reported".to_string(), |n| n.to_string())
}

// ───────────────────────────── query ─────────────────────────────

async fn run_query(
    config: &Config,
    home: &Path,
    only: Option<&str>,
    live: bool,
    json: bool,
) -> Result<()> {
    let providers = select_providers(config, only)?;

    // The library the rows come from: a throwaway one queried now (`--live`),
    // or the persisted one.
    let lib = if live {
        let client = build_client()?;
        let mut lib = ModelLibrary::new(now_rfc3339());
        for (name, cfg) in &providers {
            let outcome = query(&client, &plan_for(cfg), QUERY_TIMEOUT).await;
            lib.providers
                .insert(name.clone(), outcome_to_entry(name, cfg, outcome));
        }
        lib
    } else {
        let path = library_path(home);
        let lib = ModelLibrary::load(&path)
            .with_context(|| format!("reading model library at {}", path.display()))?;
        let Some(lib) = lib.filter(|l| !l.providers.is_empty()) else {
            let msg = format!(
                "model library is empty or missing ({}).\nRun `tars models update` to build it, \
                 or `tars models --live` to query the provider APIs directly.",
                path.display()
            );
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "command": "models", "error": msg })
                );
            } else {
                println!("{msg}");
            }
            return Ok(());
        };
        lib
    };

    let catalog = ModelCatalog::new(&MODEL_KB, Some(lib.clone()));
    // Render library rows for the selected providers, carrying the CURRENT
    // configured default (config may have changed since the last update).
    let rows: Vec<QueryRow> = providers
        .iter()
        .map(|(name, cfg)| {
            let entry = lib
                .providers
                .get(name)
                .cloned()
                .unwrap_or_else(|| ProviderEntry {
                    provider_type: provider_type_of(cfg).to_string(),
                    catalog: cfg.catalog_name(name).to_string(),
                    default_model: cfg.default_model().to_string(),
                    status: EntryStatus::Skipped,
                    models: vec![],
                    note: Some("not in library — run `tars models update`".to_string()),
                    queried_at: String::new(),
                });
            let spec = cfg.default_model().to_string();
            let resolved = catalog
                .resolve(cfg.catalog_name(name), &spec)
                .map(|r| r.id)
                .map_err(|e| e.to_string());
            let default_listed = match (&resolved, entry.status) {
                (Ok(id), EntryStatus::Ok) => Some(entry.model(id).is_some()),
                _ => None,
            };
            QueryRow {
                name: name.clone(),
                entry,
                default_spec: spec,
                default_resolved: resolved,
                default_listed,
            }
        })
        .collect();

    let age_days = (!live)
        .then(|| chrono::DateTime::parse_from_rfc3339(&lib.updated_at).ok())
        .flatten()
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_days());

    if json {
        print_query_json(live, &lib.updated_at, &rows);
    } else {
        print_query_human(live, &lib.updated_at, age_days, &rows);
    }
    Ok(())
}

struct QueryRow {
    name: String,
    entry: ProviderEntry,
    default_spec: String,
    /// The default resolved against the listed catalog, or why it didn't.
    default_resolved: Result<String, String>,
    /// Whether the resolved default is in the listed models — `None` when
    /// there's no list to check against.
    default_listed: Option<bool>,
}

impl QueryRow {
    fn default_label(&self) -> String {
        match &self.default_resolved {
            Ok(id) if *id == self.default_spec => id.clone(),
            Ok(id) => format!("{} → {id}", self.default_spec),
            Err(e) => format!("{} (does not resolve: {e})", self.default_spec),
        }
    }
}

fn print_query_human(live: bool, updated_at: &str, age_days: Option<i64>, rows: &[QueryRow]) {
    if live {
        println!("Models (live):\n");
    } else {
        println!("Models (library, updated {updated_at}):");
        if let Some(days) = age_days.filter(|d| *d >= STALE_AFTER_DAYS) {
            println!(
                "⚠ the library is {days} days old — run `tars models update` (new models and \
                 limits reach the runtime only through a refresh)"
            );
        }
        println!();
    }
    for row in rows {
        let entry = &row.entry;
        match entry.status {
            EntryStatus::Ok => {
                let stale = if row.default_listed == Some(false) {
                    "  ⚠ default not in list (stale config?)"
                } else {
                    ""
                };
                println!(
                    "  {}  ({})  [default: {}]{}",
                    row.name,
                    entry.provider_type,
                    row.default_label(),
                    stale
                );
                if entry.models.is_empty() {
                    println!("      (no models listed)");
                }
                for m in &entry.models {
                    let marker = if row.default_resolved.as_deref() == Ok(m.id.as_str()) {
                        "  ← default"
                    } else {
                        ""
                    };
                    let limits = match (m.context, m.max_output) {
                        (None, None) => String::new(),
                        (c, o) => format!("  (context {}, max output {})", fmt_opt(c), fmt_opt(o)),
                    };
                    println!("      {}{limits}{marker}", m.id);
                }
            }
            _ => {
                let note = entry.note.as_deref().unwrap_or("not available");
                println!(
                    "  {}  ({})  [default: {}] — {}",
                    row.name,
                    entry.provider_type,
                    row.default_label(),
                    note
                );
            }
        }
    }
}

fn print_query_json(live: bool, updated_at: &str, rows: &[QueryRow]) {
    let providers: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let e = &r.entry;
            serde_json::json!({
                "name": r.name,
                "type": e.provider_type,
                "catalog": e.catalog,
                "status": e.status,
                "note": e.note,
                "default_model": r.default_spec,
                "default_resolved": r.default_resolved.as_ref().ok(),
                "default_error": r.default_resolved.as_ref().err(),
                "default_listed": r.default_listed,
                "models": e.models,
                "queried_at": e.queried_at,
            })
        })
        .collect();
    let env = serde_json::json!({
        "command": "models",
        "source": if live { "live" } else { "library" },
        "updated_at": updated_at,
        "providers": providers,
    });
    println!("{}", serde_json::to_string_pretty(&env).unwrap_or_default());
}

pub fn provider_type_of(cfg: &ProviderConfig) -> &'static str {
    use ProviderConfig as P;
    match cfg {
        P::Openai { .. } => "openai",
        P::OpenaiCompat { .. } => "openai_compat",
        P::Anthropic { .. } => "anthropic",
        P::Gemini { .. } => "gemini",
        P::Bedrock { .. } => "bedrock",
        P::Vllm { .. } => "vllm",
        P::Mlx { .. } => "mlx",
        P::Llamacpp { .. } => "llamacpp",
        P::ClaudeCli { .. } => "claude_cli",
        P::ClaudeSdk { .. } => "claude_sdk",
        P::CodexCli { .. } => "codex_cli",
        P::Opencode { .. } => "opencode",
        P::Antigravity { .. } => "antigravity",
        P::Mock { .. } => "mock",
        P::Cassette { .. } => "cassette",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::{Auth, HttpProviderExtras};

    fn gemini_cfg(default: &str) -> ProviderConfig {
        ProviderConfig::Gemini {
            base_url: None,
            auth: Auth::env("GEMINI_API_KEY"),
            default_model: default.into(),
            extras: HttpProviderExtras::default(),
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
            provider_type: "gemini".into(),
            catalog: "gemini".into(),
            default_model: "flash@latest".into(),
            status: EntryStatus::Ok,
            models,
            note: None,
            queried_at: at.into(),
        }
    }

    /// Every id provider.toml's gemini block has, at the shipped limits, plus
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
        let mut lib = ModelLibrary::new(row.queried_at.clone());
        lib.providers.insert("gemini_flash".into(), row);
        lib
    }

    #[test]
    fn outcome_nokey_carries_var_name_not_a_sentinel() {
        let cfg = gemini_cfg("gemini-2.5-flash");
        let e = outcome_to_entry(
            "gemini_flash",
            &cfg,
            Outcome::NoKey {
                var: "GEMINI_API_KEY".into(),
            },
        );
        assert_eq!(e.status, EntryStatus::NoKey);
        assert_eq!(e.catalog, "gemini");
        assert_eq!(e.note.as_deref(), Some("no key: set $GEMINI_API_KEY"));
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
        let c = provider_change("gemini_flash", Some(&old), new);
        assert_eq!(c.added, vec!["b"]);
        assert_eq!(c.removed, vec!["gone"]);
        assert_eq!(
            c.limit_changes,
            vec![LimitChange {
                model: "a".into(),
                field: LimitField::MaxOutput,
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
            field: LimitField::MaxOutput,
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
        let cfg = ConfigManager::load_from_str(toml).expect("config");
        let err = select_providers(&cfg, Some("nope")).unwrap_err();
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
        let cfg = ConfigManager::load_from_str(toml).expect("config");
        let sel = select_providers(&cfg, Some("gemini_flash")).unwrap();
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].0, "gemini_flash");
    }

    #[test]
    fn provider_type_string_matches_serde_tag() {
        assert_eq!(provider_type_of(&gemini_cfg("m")), "gemini");
    }
}
