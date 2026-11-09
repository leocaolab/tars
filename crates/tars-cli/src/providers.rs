//! Runs `tars providers`: lists the configured providers with their auth and, under `--check`,
//! whether each one's model-list API answers. It never prints a secret, and every probe is
//! bounded by a timeout.

use std::sync::Arc;
use std::time::Duration;

use tars_config::{AuthSetting, ProviderConfig, ProviderKind};
use tars_llm::{EnvSource, env_credential};
use tars_types::{Auth, SecretRef};

use crate::Ending;
use crate::config::LoadConfigError;
use crate::home_env::HomeEnvError;
use crate::live_models::{ListOutcome, Plan, list_models, plan_for, with_body};

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

/// Says how a provider authenticates and, for an env-var key, whether `env` holds a usable key
/// in it.
fn auth_status(cfg: &ProviderConfig, env: &dyn EnvSource) -> AuthStatus {
    match cfg.auth() {
        AuthSetting::Configured(Auth::Secret {
            secret: SecretRef::Env { var },
        }) => AuthStatus::EnvKey {
            var: var.clone(),
            set: env_credential(env, var).is_ok(),
        },
        AuthSetting::Configured(Auth::Secret {
            secret: SecretRef::Inline { .. },
        }) => AuthStatus::InlineKey,
        AuthSetting::Configured(Auth::Secret {
            secret: SecretRef::File { .. },
        }) => AuthStatus::FileKey,
        AuthSetting::Configured(Auth::Delegate) => AuthStatus::Delegated,
        AuthSetting::Configured(Auth::None) | AuthSetting::NoCall => AuthStatus::Keyless,
        AuthSetting::OwnLogin => AuthStatus::ToolLogin,
    }
}

/// How a provider authenticates, as `tars providers` reports it.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AuthStatus {
    /// The key is read from the variable `var`; `set` says whether it holds a usable key.
    #[serde(rename = "env")]
    EnvKey { var: String, set: bool },
    #[serde(rename = "inline")]
    InlineKey,
    #[serde(rename = "file")]
    FileKey,
    /// The config says `auth = "delegate"`: the tool uses the credential it already has.
    #[serde(rename = "delegate")]
    Delegated,
    /// The provider type has no `auth` setting: the CLI or daemon logs in on its own.
    ToolLogin,
    /// The provider does not authenticate: the config says `auth = "none"`, or it places no
    /// call (mock, cassette).
    #[serde(rename = "none")]
    Keyless,
}

impl AuthStatus {
    fn human(&self) -> String {
        match self {
            AuthStatus::EnvKey { var, set: true } => format!("key: ${var} (set)"),
            AuthStatus::EnvKey { var, set: false } => format!("key: ${var} (UNSET)"),
            AuthStatus::InlineKey => "key: inline".to_string(),
            AuthStatus::FileKey => "key: file".to_string(),
            AuthStatus::Delegated => "auth: delegated to tool login".to_string(),
            AuthStatus::ToolLogin => "auth: the tool's own login".to_string(),
            AuthStatus::Keyless => "auth: none (keyless)".to_string(),
        }
    }
}

/// Why `tars providers` did not list, or which providers failed `--check`.
#[derive(Debug, thiserror::Error)]
pub enum ProvidersError {
    #[error(transparent)]
    Config(#[from] LoadConfigError),
    #[error(transparent)]
    Env(#[from] HomeEnvError),
    #[error("building HTTP client")]
    Client(#[source] reqwest::Error),
    #[error("writing the list as JSON")]
    Json(#[source] serde_json::Error),
    #[error("{} of {checked} providers failed the check: {}", failed.len(), failed.join(", "))]
    CheckFailed { failed: Vec<String>, checked: usize },
}

impl ProvidersError {
    /// Says whether the command ran: a check that asked every provider ran, and the providers
    /// that did not answer are what it found.
    pub(crate) fn ending(&self) -> Ending {
        match self {
            Self::CheckFailed { .. } => Ending::Failed,
            Self::Config(_) | Self::Env(_) | Self::Client(_) | Self::Json(_) => Ending::NotRun,
        }
    }
}

pub async fn execute(
    args: ProvidersArgs,
    config_paths: &crate::config::ConfigPaths,
) -> Result<(), ProvidersError> {
    let config = crate::config::load(config_paths)?.config;
    let env: Arc<dyn EnvSource> = Arc::new(crate::home_env::load()?);

    let mut providers: Vec<(String, &ProviderConfig)> = config
        .providers()
        .iter()
        .map(|(id, cfg)| (id.as_str().to_string(), cfg))
        .collect();
    providers.sort_by(|a, b| a.0.cmp(&b.0));

    let client = if args.check {
        Some(crate::live_models::client(Duration::from_secs(4)).map_err(ProvidersError::Client)?)
    } else {
        None
    };

    let mut rows: Vec<ProviderRow> = Vec::with_capacity(providers.len());
    for (name, cfg) in &providers {
        let reach = match &client {
            Some(client) => Some(match plan_for(cfg) {
                Plan::Unlisted { note } => Reach::NotApplicable { note },
                Plan::Http(plan) => {
                    Reach::Asked(list_models(client, &plan, CHECK_TIMEOUT, &env).await)
                }
            }),
            None => None,
        };
        rows.push(ProviderRow {
            name,
            provider_type: cfg.kind(),
            default_model: cfg.default_model(),
            auth: auth_status(cfg, env.as_ref()),
            reach,
        });
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&render_json(&rows)).map_err(ProvidersError::Json)?
        );
    } else {
        print!("{}", render_text(&rows));
    }
    let failed: Vec<String> = rows
        .iter()
        .filter(|r| r.reach.as_ref().is_some_and(Reach::is_failure))
        .map(|r| r.name.to_string())
        .collect();
    if !failed.is_empty() {
        return Err(ProvidersError::CheckFailed {
            failed,
            checked: rows.len(),
        });
    }
    Ok(())
}

/// One configured provider as `tars providers` reports it.
struct ProviderRow<'a> {
    name: &'a str,
    provider_type: ProviderKind,
    default_model: &'a str,
    auth: AuthStatus,
    /// How the `--check` probe went; `None` without `--check`.
    reach: Option<Reach>,
}

/// Returns the text report: a heading, then two lines per provider.
fn render_text(rows: &[ProviderRow]) -> String {
    let mut out = String::from("Configured providers:\n\n");
    for row in rows {
        let reach = row
            .reach
            .as_ref()
            .map(|r| format!("  |  {}", r.human()))
            .unwrap_or_default();
        out.push_str(&format!(
            "  {}  ({})  [default: {}]\n      {}{reach}\n",
            row.name,
            row.provider_type,
            row.default_model,
            row.auth.human()
        ));
    }
    out
}

/// Returns the JSON report: one object per provider, `reachability` and its `detail` only under
/// `--check`.
fn render_json(rows: &[ProviderRow]) -> serde_json::Value {
    let providers: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "name": row.name,
                "type": row.provider_type,
                "default_model": row.default_model,
                "auth": row.auth,
                "reachability": row.reach.as_ref().map(Reach::as_str),
                "detail": row.reach.as_ref().and_then(Reach::detail),
            })
        })
        .collect();
    serde_json::json!({ "command": "providers", "providers": providers })
}

/// How a provider's `--check` probe went.
enum Reach {
    /// The provider has no list API, with the reason.
    NotApplicable {
        note: String,
    },
    Asked(ListOutcome),
}

impl Reach {
    /// Says whether the provider failed the check. One whose key variable is not set, that has
    /// no list API, or whose self-hosted server is not running was never asked; every other
    /// answer but a list is a failure, a 2xx body that does not parse included.
    fn is_failure(&self) -> bool {
        match self {
            Reach::NotApplicable { .. } => false,
            Reach::Asked(outcome) => match outcome {
                ListOutcome::Ok { .. }
                | ListOutcome::NoKey { .. }
                | ListOutcome::NotRunning { .. } => false,
                ListOutcome::KeyUnresolved { .. }
                | ListOutcome::KeyInvalid { .. }
                | ListOutcome::AuthFailed { .. }
                | ListOutcome::HttpStatus { .. }
                | ListOutcome::Unreachable { .. }
                | ListOutcome::ParseError { .. } => true,
            },
        }
    }

    /// Returns the probe's verdict word, stable for machine readers.
    fn as_str(&self) -> &'static str {
        match self {
            Reach::NotApplicable { .. } => "not_applicable",
            Reach::Asked(outcome) => match outcome {
                ListOutcome::Ok { .. } => "reachable",
                ListOutcome::NoKey { .. } => "no_key",
                ListOutcome::KeyUnresolved { .. } => "key_unresolved",
                ListOutcome::KeyInvalid { .. } => "key_invalid",
                ListOutcome::AuthFailed { .. } => "auth_failed",
                ListOutcome::HttpStatus { .. } => "http_error",
                ListOutcome::Unreachable { .. } => "unreachable",
                ListOutcome::NotRunning { .. } => "not_running",
                ListOutcome::ParseError { .. } => "unparsable",
            },
        }
    }

    /// Returns what the probe found beyond its verdict: the reason, the status and the server's
    /// body; `None` for a list.
    fn detail(&self) -> Option<String> {
        match self {
            Reach::NotApplicable { note } => Some(note.clone()),
            Reach::Asked(outcome) => match outcome {
                ListOutcome::Ok { .. } => None,
                ListOutcome::NoKey { var } => Some(format!("${var} is not set")),
                ListOutcome::KeyUnresolved { reason } => Some(reason.clone()),
                ListOutcome::KeyInvalid { header } => {
                    Some(format!("the key is not a valid `{header}` header value"))
                }
                ListOutcome::AuthFailed { status, body }
                | ListOutcome::HttpStatus { status, body } => {
                    Some(with_body(format!("HTTP {status}"), body))
                }
                ListOutcome::Unreachable { detail }
                | ListOutcome::NotRunning { detail }
                | ListOutcome::ParseError { detail } => Some(detail.clone()),
            },
        }
    }

    fn human(&self) -> String {
        let head = match self {
            Reach::NotApplicable { .. } => "no list API",
            Reach::Asked(outcome) => match outcome {
                ListOutcome::Ok { .. } => "reachable",
                ListOutcome::NoKey { .. } => "no key (skipped probe)",
                ListOutcome::KeyUnresolved { .. } => "key did not resolve",
                ListOutcome::KeyInvalid { .. } => "key invalid",
                ListOutcome::AuthFailed { .. } => "auth failed",
                ListOutcome::HttpStatus { .. } => "HTTP error",
                ListOutcome::Unreachable { .. } => "unreachable",
                ListOutcome::NotRunning { .. } => "server not running (could not connect)",
                ListOutcome::ParseError { .. } => "answered, but the list did not parse",
            },
        };
        match self.detail() {
            Some(detail) => format!("{head}: {detail}"),
            None => head.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_status_reflects_env_presence() {
        let cfg = ProviderConfig::Gemini {
            base_url: None,
            auth: Auth::env("TARS_TEST_DEFINITELY_UNSET_KEY_XYZ"),
            default_model: "gemini-2.5-flash".into(),
            extras: tars_types::HttpOverrides::default(),
        };
        match auth_status(&cfg, &tars_llm::ProcessEnv) {
            AuthStatus::EnvKey { var, set } => {
                assert_eq!(var, "TARS_TEST_DEFINITELY_UNSET_KEY_XYZ");
                assert!(!set);
            }
            _ => panic!("expected env key status"),
        }
    }

    #[test]
    fn a_provider_configured_with_auth_none_is_keyless() {
        let cfg = ProviderConfig::OpenaiCompat {
            base_url: "http://localhost:1234/v1".into(),
            auth: Auth::None,
            default_model: "m".into(),
            gen_ai_provider_name: "local".into(),
            extras: tars_types::HttpOverrides::default(),
            capabilities: Default::default(),
        };
        assert_eq!(
            auth_status(&cfg, &tars_llm::ProcessEnv),
            AuthStatus::Keyless
        );
    }

    #[test]
    fn a_cli_provider_has_no_auth_field_and_logs_in_on_its_own() {
        let cfg = ProviderConfig::Antigravity {
            executable: "agy".into(),
            timeout_secs: 300,
            default_model: "gemini-3.1-pro".into(),
            effort: tars_config::AntigravityEffortConfig::High,
        };
        assert_eq!(
            auth_status(&cfg, &tars_llm::ProcessEnv),
            AuthStatus::ToolLogin
        );
    }

    #[test]
    fn a_provider_that_places_no_call_is_keyless() {
        let cfg = ProviderConfig::Mock {
            canned_response: "ok".into(),
        };
        assert_eq!(
            auth_status(&cfg, &tars_llm::ProcessEnv),
            AuthStatus::Keyless
        );
    }

    struct OneVar(&'static str, &'static str);

    impl EnvSource for OneVar {
        fn var(&self, name: &str) -> Result<String, std::env::VarError> {
            if name == self.0 {
                Ok(self.1.to_string())
            } else {
                Err(std::env::VarError::NotPresent)
            }
        }
    }

    #[test]
    fn auth_status_reads_the_env_it_is_given_and_a_blank_key_is_unset() {
        let cfg = ProviderConfig::Gemini {
            base_url: None,
            auth: Auth::env("TARS_K"),
            default_model: "gemini-2.5-flash".into(),
            extras: tars_types::HttpOverrides::default(),
        };
        for (value, set) in [("sk-x", true), ("  ", false)] {
            match auth_status(&cfg, &OneVar("TARS_K", value)) {
                AuthStatus::EnvKey { set: got, .. } => assert_eq!(got, set, "{value:?}"),
                _ => panic!("expected env key status"),
            }
        }
    }

    #[test]
    fn a_list_that_does_not_parse_fails_the_check_and_says_why() {
        let reach = Reach::Asked(ListOutcome::ParseError {
            detail: "missing `data` array".into(),
        });
        assert!(reach.is_failure());
        assert_eq!(reach.as_str(), "unparsable");
        assert!(
            reach.human().contains("missing `data` array"),
            "{}",
            reach.human()
        );
    }

    #[test]
    fn only_an_unset_key_variable_no_list_api_and_a_stopped_local_server_pass_without_a_list() {
        let passing = [
            Reach::NotApplicable { note: "CLI".into() },
            Reach::Asked(ListOutcome::Ok { models: vec![] }),
            Reach::Asked(ListOutcome::NoKey { var: "K".into() }),
            Reach::Asked(ListOutcome::NotRunning {
                detail: "could not connect".into(),
            }),
        ];
        for reach in &passing {
            assert!(!reach.is_failure(), "{}", reach.as_str());
        }
        let failing = [
            Reach::Asked(ListOutcome::KeyUnresolved {
                reason: "io".into(),
            }),
            Reach::Asked(ListOutcome::KeyInvalid {
                header: "x-api-key",
            }),
            Reach::Asked(ListOutcome::Unreachable {
                detail: "refused".into(),
            }),
        ];
        for reach in &failing {
            assert!(reach.is_failure(), "{}", reach.as_str());
        }
    }

    #[test]
    fn the_json_names_how_each_provider_authenticates() {
        for (status, kind) in [
            (AuthStatus::InlineKey, "inline"),
            (AuthStatus::FileKey, "file"),
            (AuthStatus::Delegated, "delegate"),
            (AuthStatus::ToolLogin, "tool_login"),
            (AuthStatus::Keyless, "none"),
        ] {
            assert_eq!(
                serde_json::to_value(&status).unwrap(),
                serde_json::json!({ "kind": kind })
            );
        }
        assert_eq!(
            serde_json::to_value(AuthStatus::EnvKey {
                var: "K".into(),
                set: true
            })
            .unwrap(),
            serde_json::json!({ "kind": "env", "var": "K", "set": true })
        );
    }

    #[test]
    fn a_refused_check_shows_the_status_and_the_servers_body() {
        let reach = Reach::Asked(ListOutcome::AuthFailed {
            status: 403,
            body: crate::live_models::Body::Read("API key not valid".into()),
        });
        assert_eq!(reach.human(), "auth failed: HTTP 403: API key not valid");
        assert_eq!(
            reach.detail().as_deref(),
            Some("HTTP 403: API key not valid")
        );
    }
}
