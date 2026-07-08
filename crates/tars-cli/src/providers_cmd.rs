//! `tars providers` — list configured providers and their health.
//!
//! For every configured provider: name, `type`, `default_model`, and key
//! health (which env var backs its auth + whether that var is set). With
//! `--check`, also does a fast, best-effort reachability probe (the same
//! list-models GET as `tars models --live`) → reachable / auth-failed /
//! unreachable. Never prints a secret; never hangs (bounded timeout).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tars_config::{ConfigManager, ProviderConfig};
use tars_types::{Auth, SecretRef};

use crate::model_query::{plan_for, query, Outcome, Plan};
use crate::models::provider_type_of;

const CHECK_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, clap::Args)]
pub struct ProvidersArgs {
    /// Also probe each API provider for reachability (fast, best-effort).
    #[arg(long)]
    check: bool,

    /// Emit a machine-readable JSON envelope instead of human text.
    #[arg(long)]
    json: bool,
}

/// The env var backing a provider's auth, and whether it is currently set.
/// `None` env var = keyless (local / delegate / none).
fn key_status(cfg: &ProviderConfig) -> KeyStatus {
    let auth = auth_of(cfg);
    match auth {
        Some(Auth::Secret {
            secret: SecretRef::Env { var },
        }) => {
            let set = std::env::var(var).map(|v| !v.trim().is_empty()).unwrap_or(false);
            KeyStatus::Env {
                var: var.clone(),
                set,
            }
        }
        Some(Auth::Secret {
            secret: SecretRef::Inline { .. },
        }) => KeyStatus::Inline,
        Some(Auth::Secret {
            secret: SecretRef::File { .. },
        }) => KeyStatus::File,
        Some(Auth::Delegate) => KeyStatus::Delegate,
        Some(Auth::None) | None => KeyStatus::None,
    }
}

/// Borrow the `Auth` of any provider variant that has one. Keyless variants
/// (bedrock, CLI, mock, cassette) return `None`.
fn auth_of(cfg: &ProviderConfig) -> Option<&Auth> {
    use ProviderConfig as P;
    match cfg {
        P::Openai { auth, .. }
        | P::OpenaiCompat { auth, .. }
        | P::Anthropic { auth, .. }
        | P::Gemini { auth, .. }
        | P::Vllm { auth, .. }
        | P::Mlx { auth, .. }
        | P::Llamacpp { auth, .. } => Some(auth),
        _ => None,
    }
}

enum KeyStatus {
    Env { var: String, set: bool },
    Inline,
    File,
    Delegate,
    None,
}

impl KeyStatus {
    fn human(&self) -> String {
        match self {
            KeyStatus::Env { var, set: true } => format!("key: ${var} (set)"),
            KeyStatus::Env { var, set: false } => format!("key: ${var} (UNSET)"),
            KeyStatus::Inline => "key: inline".to_string(),
            KeyStatus::File => "key: file".to_string(),
            KeyStatus::Delegate => "auth: delegated to tool login".to_string(),
            KeyStatus::None => "auth: none (keyless)".to_string(),
        }
    }

    fn as_json(&self) -> serde_json::Value {
        match self {
            KeyStatus::Env { var, set } => {
                serde_json::json!({ "kind": "env", "var": var, "set": set })
            }
            KeyStatus::Inline => serde_json::json!({ "kind": "inline" }),
            KeyStatus::File => serde_json::json!({ "kind": "file" }),
            KeyStatus::Delegate => serde_json::json!({ "kind": "delegate" }),
            KeyStatus::None => serde_json::json!({ "kind": "none" }),
        }
    }
}

pub async fn execute(args: ProvidersArgs, config_flag: Option<PathBuf>) -> Result<()> {
    let home = tars_config::resolve_home(None)
        .context("cannot resolve tars home (set $TARS_HOME or ensure HOME is set)")?;
    let _ = dotenvy::from_path(home.join(".env"));

    let config_path = config_flag.unwrap_or_else(|| home.join("config.toml"));
    let config = ConfigManager::load_from_file(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let mut providers: Vec<(String, &ProviderConfig)> = config
        .providers
        .iter()
        .map(|(id, cfg)| (id.as_str().to_string(), cfg))
        .collect();
    providers.sort_by(|a, b| a.0.cmp(&b.0));

    let client = if args.check {
        Some(
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(4))
                .build()
                .context("building HTTP client")?,
        )
    } else {
        None
    };

    let mut rows: Vec<serde_json::Value> = Vec::new();
    if !args.json {
        println!("Configured providers:\n");
    }
    for (name, cfg) in &providers {
        let ptype = provider_type_of(cfg);
        let key = key_status(cfg);
        let default_model = cfg.default_model();

        // Reachability probe (only with --check, only for HTTP providers).
        let reach = if let Some(client) = &client {
            match plan_for(cfg) {
                Plan::Skip { .. } => Some(Reach::NotApplicable),
                plan @ Plan::Http(_) => Some(match query(client, &plan, CHECK_TIMEOUT).await {
                    Outcome::Ok { .. } => Reach::Ok,
                    Outcome::NoKey { .. } => Reach::NoKey,
                    Outcome::AuthFailed { status } => Reach::AuthFailed(status),
                    Outcome::HttpStatus { status } => Reach::HttpStatus(status),
                    Outcome::Unreachable { .. } => Reach::Unreachable,
                    Outcome::ParseError { .. } => Reach::Ok, // reached + 2xx
                    Outcome::Skipped { .. } => Reach::NotApplicable,
                }),
            }
        } else {
            None
        };

        if args.json {
            rows.push(serde_json::json!({
                "name": name,
                "type": ptype,
                "default_model": default_model,
                "auth": key.as_json(),
                "reachability": reach.as_ref().map(Reach::as_str),
            }));
        } else {
            let reach_str = reach
                .as_ref()
                .map(|r| format!("  |  {}", r.human()))
                .unwrap_or_default();
            println!(
                "  {name}  ({ptype})  [default: {default_model}]\n      {}{reach_str}",
                key.human()
            );
        }
    }

    if args.json {
        let env = serde_json::json!({ "command": "providers", "providers": rows });
        println!("{}", serde_json::to_string_pretty(&env).unwrap_or_default());
    }
    Ok(())
}

enum Reach {
    Ok,
    NoKey,
    AuthFailed(u16),
    HttpStatus(u16),
    Unreachable,
    NotApplicable,
}

impl Reach {
    fn human(&self) -> String {
        match self {
            Reach::Ok => "reachable".to_string(),
            Reach::NoKey => "no key (skipped probe)".to_string(),
            Reach::AuthFailed(s) => format!("auth failed (HTTP {s})"),
            Reach::HttpStatus(s) => format!("HTTP {s}"),
            Reach::Unreachable => "unreachable".to_string(),
            Reach::NotApplicable => "no list API (CLI/bedrock/mock)".to_string(),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Reach::Ok => "reachable",
            Reach::NoKey => "no_key",
            Reach::AuthFailed(_) => "auth_failed",
            Reach::HttpStatus(_) => "http_error",
            Reach::Unreachable => "unreachable",
            Reach::NotApplicable => "not_applicable",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_status_reflects_env_presence() {
        // A unique var name unlikely to be set in the test env.
        let cfg = ProviderConfig::Gemini {
            base_url: None,
            auth: Auth::env("TARS_TEST_DEFINITELY_UNSET_KEY_XYZ"),
            default_model: "gemini-2.5-flash".into(),
            extras: tars_types::HttpProviderExtras::default(),
        };
        match key_status(&cfg) {
            KeyStatus::Env { var, set } => {
                assert_eq!(var, "TARS_TEST_DEFINITELY_UNSET_KEY_XYZ");
                assert!(!set);
            }
            _ => panic!("expected env key status"),
        }
    }

    #[test]
    fn keyless_local_reports_none() {
        let cfg = ProviderConfig::OpenaiCompat {
            base_url: "http://localhost:1234/v1".into(),
            auth: Auth::None,
            default_model: "m".into(),
            extras: tars_types::HttpProviderExtras::default(),
            capabilities: Default::default(),
        };
        assert!(matches!(key_status(&cfg), KeyStatus::None));
    }

    #[test]
    fn cli_provider_has_no_auth_field() {
        let cfg = ProviderConfig::GeminiCli {
            executable: "gemini".into(),
            timeout_secs: 300,
            default_model: "gemini-3-flash-preview".into(),
        };
        assert!(matches!(key_status(&cfg), KeyStatus::None));
    }
}
