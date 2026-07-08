//! `tars models` — discover provider models over a persisted **model library**.
//!
//! Two actions:
//!   - `tars models [PROVIDER]`         — QUERY: read the library (fast, offline).
//!     `--live` bypasses the library and hits the provider APIs directly.
//!   - `tars models update [PROVIDER]`  — UPDATE: refresh the library from the
//!     live APIs, report what changed, and flag any stale `default_model`.
//!
//! The CLI stays thin: classification + parsing live in [`crate::model_query`],
//! persistence + diff in [`crate::model_library`]. This module is orchestration
//! and rendering only.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tars_config::{Config, ConfigManager, ProviderConfig};
use tars_types::ProviderId;

use crate::model_library::{
    diff_models, library_path, EntryStatus, ModelLibrary, ProviderEntry,
};
use crate::model_query::{plan_for, query, Outcome};

/// Per-request budget for a live model-list query. Bounds each provider so a
/// dead local server or a hung TLS handshake can't stall the command.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Refresh the model library from the live provider APIs, report what
    /// changed since last time, and flag any stale `default_model`.
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
        None => run_query(&config, &home, args.provider.as_deref(), args.live, args.json).await,
    }
}

/// Collect the configured providers to act on, sorted by name. Errors if a
/// requested `PROVIDER` isn't configured.
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
                    let names: Vec<&str> =
                        all.iter().map(|(n, _)| n.as_str()).collect();
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

/// Turn a live [`Outcome`] into a persisted [`ProviderEntry`].
fn outcome_to_entry(provider_type: &str, default_model: &str, outcome: Outcome) -> ProviderEntry {
    let queried_at = now_rfc3339();
    let base = |status, models, note| ProviderEntry {
        provider_type: provider_type.to_string(),
        default_model: default_model.to_string(),
        status,
        models,
        note,
        queried_at: queried_at.clone(),
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
            Some(format!("auth rejected (HTTP {status}) — key invalid/expired?")),
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

// ───────────────────────────── update ─────────────────────────────

async fn run_update(
    config: &Config,
    home: &std::path::Path,
    only: Option<&str>,
    json: bool,
) -> Result<()> {
    let providers = select_providers(config, only)?;
    let path = library_path(home);
    let old = ModelLibrary::load(&path)
        .with_context(|| format!("reading existing model library at {}", path.display()))?;
    let client = build_client()?;

    // Query each selected provider live.
    let mut fresh: BTreeMap<String, ProviderEntry> = BTreeMap::new();
    for (name, cfg) in &providers {
        let plan = plan_for(cfg);
        let outcome = query(&client, &plan, QUERY_TIMEOUT).await;
        fresh.insert(
            name.clone(),
            outcome_to_entry(provider_type_of(cfg), cfg.default_model(), outcome),
        );
    }

    // Merge into the prior library: a single-provider update must not drop the
    // rows for providers it didn't touch.
    let mut lib = old.clone().unwrap_or_else(|| ModelLibrary::new(now_rfc3339()));
    lib.updated_at = now_rfc3339();
    lib.version = crate::model_library::LIBRARY_VERSION;
    for (name, entry) in &fresh {
        lib.providers.insert(name.clone(), entry.clone());
    }
    lib.save(&path)
        .with_context(|| format!("writing model library to {}", path.display()))?;

    // Diff each freshly-queried provider against the prior library.
    let old_ref = old.as_ref();
    let mut changes: Vec<ProviderChange> = Vec::new();
    for (name, entry) in &fresh {
        let prev = old_ref.and_then(|l| l.providers.get(name));
        let (added, removed) = match (prev, entry.status) {
            (Some(p), EntryStatus::Ok) => diff_models(&p.models, &entry.models),
            // No prior Ok row → everything currently listed is "added".
            (_, EntryStatus::Ok) => (entry.models.clone(), vec![]),
            _ => (vec![], vec![]),
        };
        changes.push(ProviderChange {
            name: name.clone(),
            entry: entry.clone(),
            added,
            removed,
        });
    }

    if json {
        print_update_json(&path, &changes);
    } else {
        print_update_human(&path, &changes);
    }
    Ok(())
}

struct ProviderChange {
    name: String,
    entry: ProviderEntry,
    added: Vec<String>,
    removed: Vec<String>,
}

fn print_update_human(path: &std::path::Path, changes: &[ProviderChange]) {
    println!("Updated model library: {}\n", path.display());
    let mut stale_warnings: Vec<String> = Vec::new();

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
                    println!("      - removed: {}  (deprecated/retired)", c.removed.join(", "));
                }
                if c.added.is_empty() && c.removed.is_empty() {
                    println!("      (no change)");
                }
                if c.entry.default_is_stale() {
                    stale_warnings.push(format!(
                        "  {}: configured default_model '{}' is NOT in the live list \
                         (stale config — update .arc/tars config)",
                        c.name, c.entry.default_model
                    ));
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

    if !stale_warnings.is_empty() {
        println!("\n⚠ stale default_model (config not auto-edited — fix by hand):");
        for w in &stale_warnings {
            println!("{w}");
        }
    }
}

fn print_update_json(path: &std::path::Path, changes: &[ProviderChange]) {
    let providers: Vec<serde_json::Value> = changes
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "type": c.entry.provider_type,
                "status": c.entry.status,
                "note": c.entry.note,
                "model_count": c.entry.models.len(),
                "added": c.added,
                "removed": c.removed,
                "default_model": c.entry.default_model,
                "default_stale": c.entry.default_is_stale(),
            })
        })
        .collect();
    let env = serde_json::json!({
        "command": "models update",
        "library": path.display().to_string(),
        "providers": providers,
    });
    println!("{}", serde_json::to_string_pretty(&env).unwrap_or_default());
}

// ───────────────────────────── query ─────────────────────────────

async fn run_query(
    config: &Config,
    home: &std::path::Path,
    only: Option<&str>,
    live: bool,
    json: bool,
) -> Result<()> {
    let providers = select_providers(config, only)?;

    // Build the rows to render: each is (name, ProviderEntry). Live mode
    // queries the APIs; cached mode reads the library.
    let rows: Vec<(String, ProviderEntry)> = if live {
        let client = build_client()?;
        let mut out = Vec::new();
        for (name, cfg) in &providers {
            let outcome = query(&client, &plan_for(cfg), QUERY_TIMEOUT).await;
            out.push((
                name.clone(),
                outcome_to_entry(provider_type_of(cfg), cfg.default_model(), outcome),
            ));
        }
        out
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
        // Render library rows, but only for the selected providers, and carry
        // the CURRENT configured default_model (config may have changed since
        // the last update).
        providers
            .iter()
            .map(|(name, cfg)| {
                let entry = lib.providers.get(name).cloned().unwrap_or_else(|| {
                    ProviderEntry {
                        provider_type: provider_type_of(cfg).to_string(),
                        default_model: cfg.default_model().to_string(),
                        status: EntryStatus::Skipped,
                        models: vec![],
                        note: Some("not in library — run `tars models update`".to_string()),
                        queried_at: String::new(),
                    }
                });
                // Reflect the live config default (not the one snapshotted at
                // update time) so the stale check tracks current config.
                let entry = ProviderEntry {
                    default_model: cfg.default_model().to_string(),
                    ..entry
                };
                (name.clone(), entry)
            })
            .collect()
    };

    if json {
        print_query_json(live, &rows);
    } else {
        print_query_human(live, &rows);
    }
    Ok(())
}

fn print_query_human(live: bool, rows: &[(String, ProviderEntry)]) {
    let source = if live { "live" } else { "library" };
    println!("Models ({source}):\n");
    for (name, entry) in rows {
        match entry.status {
            EntryStatus::Ok => {
                let stale = if entry.default_is_stale() {
                    "  ⚠ default not in list (stale config?)"
                } else {
                    ""
                };
                println!(
                    "  {}  ({})  [default: {}]{}",
                    name, entry.provider_type, entry.default_model, stale
                );
                if entry.models.is_empty() {
                    println!("      (no models listed)");
                }
                for m in &entry.models {
                    let marker = if m == &entry.default_model { "  ← default" } else { "" };
                    println!("      {m}{marker}");
                }
            }
            _ => {
                let note = entry.note.as_deref().unwrap_or("not available");
                println!(
                    "  {}  ({})  [default: {}] — {}",
                    name, entry.provider_type, entry.default_model, note
                );
            }
        }
    }
}

fn print_query_json(live: bool, rows: &[(String, ProviderEntry)]) {
    let providers: Vec<serde_json::Value> = rows
        .iter()
        .map(|(name, e)| {
            serde_json::json!({
                "name": name,
                "type": e.provider_type,
                "status": e.status,
                "note": e.note,
                "default_model": e.default_model,
                "default_stale": e.default_is_stale(),
                "models": e.models,
                "queried_at": e.queried_at,
            })
        })
        .collect();
    let env = serde_json::json!({
        "command": "models",
        "source": if live { "live" } else { "library" },
        "providers": providers,
    });
    println!("{}", serde_json::to_string_pretty(&env).unwrap_or_default());
}

/// The provider `type` string, for display/persistence. Mirrors the serde tag
/// on [`ProviderConfig`].
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
        P::GeminiCli { .. } => "gemini_cli",
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

    #[test]
    fn outcome_ok_becomes_ok_entry() {
        let e = outcome_to_entry(
            "gemini",
            "gemini-2.5-flash",
            Outcome::Ok {
                models: vec!["gemini-2.5-flash".into(), "gemini-2.5-pro".into()],
            },
        );
        assert_eq!(e.status, EntryStatus::Ok);
        assert_eq!(e.models.len(), 2);
        assert!(!e.default_is_stale());
    }

    #[test]
    fn outcome_nokey_carries_var_name_not_a_sentinel() {
        let e = outcome_to_entry(
            "gemini",
            "gemini-2.5-flash",
            Outcome::NoKey {
                var: "GEMINI_API_KEY".into(),
            },
        );
        assert_eq!(e.status, EntryStatus::NoKey);
        assert_eq!(e.note.as_deref(), Some("no key: set $GEMINI_API_KEY"));
    }

    #[test]
    fn stale_default_surfaces_when_default_absent_from_live_list() {
        let e = outcome_to_entry(
            "gemini",
            "gemini-3-flash-preview", // leftover preview no longer offered
            Outcome::Ok {
                models: vec!["gemini-2.5-flash".into(), "gemini-3.1-flash".into()],
            },
        );
        assert!(e.default_is_stale());
    }

    #[test]
    fn select_providers_errors_on_unknown_name() {
        let toml = r#"
            [providers.gemini_flash]
            type = "gemini"
            default_model = "gemini-2.5-flash"
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
            default_model = "gemini-2.5-flash"
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
