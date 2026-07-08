//! Per-provider live model-list queries for `tars models`.
//!
//! Two halves, split so the fiddly half is unit-testable without a network:
//!
//! - [`plan_for`] — pure classification: given a [`ProviderConfig`], decide
//!   *how* (if at all) its available models can be listed. Each HTTP provider
//!   `type` maps to a REST endpoint + auth placement + response shape:
//!     - `gemini`                → `{base}/v1beta/models?key=…`   (Gemini shape)
//!     - `openai`                → `{base}/models`  Bearer         (OpenAI shape)
//!     - `openai_compat`/`vllm`/`mlx`/`llamacpp` → `{base}/models` Bearer (local, key optional)
//!     - `anthropic`             → `{base}/v1/models`  x-api-key + anthropic-version
//!
//!   CLI / bedrock / mock / cassette have no list API → [`Plan::Skip`].
//! - [`parse_models`] — pure response parsing per [`ParseStyle`], covered by
//!   unit tests against captured sample JSON (no network in tests).
//!
//! [`query`] glues them: resolve the key from the provider's auth env var,
//! fire one GET with a timeout, classify the outcome. Never panics, never
//! prints (or returns) the key, never hangs (bounded by the reqwest timeout).

use std::time::Duration;

use tars_config::ProviderConfig;
use tars_types::{Auth, SecretRef};

/// Default endpoints, mirrored from the `tars-provider` backends so the
/// query hits the same host the runtime would.
const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com";
const OPENAI_BASE: &str = "https://api.openai.com/v1";
const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MLX_BASE: &str = "http://localhost:8080/v1";
const VLLM_BASE: &str = "http://localhost:8000/v1";
const LLAMACPP_BASE: &str = "http://localhost:8080/v1";

/// Shape of the provider's list-models JSON response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseStyle {
    /// Gemini: `{ "models": [ { "name": "models/gemini-2.5-flash", … } ] }`.
    /// The `models/` prefix is stripped.
    Gemini,
    /// OpenAI / OpenAI-compatible / Anthropic: `{ "data": [ { "id": "…" } ] }`.
    OpenAiData,
}

/// Where the API key rides on the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthMode {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// `x-api-key: <key>` + `anthropic-version: <version>`.
    XApiKey { version: String },
    /// Gemini's `?key=<key>` query parameter.
    GeminiQuery,
}

/// A concrete plan to list one provider's models over HTTP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpPlan {
    /// Full URL, WITHOUT any key (the Gemini `?key=` is appended at send time
    /// so the key never lives in a stored/logged string).
    pub url: String,
    pub auth: AuthMode,
    pub parse: ParseStyle,
    /// The configured auth env-var name, if this provider authenticates via a
    /// secret env var. `None` = keyless (local server) — send no credential.
    pub env_var: Option<String>,
}

/// How a provider's models can be discovered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Queryable over HTTP.
    Http(HttpPlan),
    /// Not queryable (CLI provider, bedrock, mock, cassette) — with a
    /// human-readable reason to show verbatim.
    Skip { note: String },
}

/// The env-var name backing a provider's auth, if any. `None` for
/// `Auth::None` / `Auth::Delegate` / inline / file secrets — the caller
/// treats that as "no env key to resolve".
fn env_var_of(auth: &Auth) -> Option<String> {
    match auth {
        Auth::Secret {
            secret: SecretRef::Env { var },
        } => Some(var.clone()),
        _ => None,
    }
}

fn trim_base(base: &str) -> &str {
    base.trim_end_matches('/')
}

/// Classify a provider into a model-list [`Plan`]. Pure — no I/O.
pub fn plan_for(cfg: &ProviderConfig) -> Plan {
    use ProviderConfig as P;
    match cfg {
        P::Gemini { base_url, auth, .. } => {
            let base = trim_base(base_url.as_deref().unwrap_or(GEMINI_BASE));
            Plan::Http(HttpPlan {
                url: format!("{base}/v1beta/models"),
                auth: AuthMode::GeminiQuery,
                parse: ParseStyle::Gemini,
                env_var: env_var_of(auth),
            })
        }
        P::Openai { base_url, auth, .. } => {
            let base = trim_base(base_url.as_deref().unwrap_or(OPENAI_BASE));
            Plan::Http(HttpPlan {
                url: format!("{base}/models"),
                auth: AuthMode::Bearer,
                parse: ParseStyle::OpenAiData,
                env_var: env_var_of(auth),
            })
        }
        P::Anthropic {
            base_url,
            api_version,
            auth,
            ..
        } => {
            let base = trim_base(base_url.as_deref().unwrap_or(ANTHROPIC_BASE));
            let version = api_version
                .clone()
                .unwrap_or_else(|| ANTHROPIC_VERSION.to_string());
            Plan::Http(HttpPlan {
                url: format!("{base}/v1/models"),
                auth: AuthMode::XApiKey { version },
                parse: ParseStyle::OpenAiData,
                env_var: env_var_of(auth),
            })
        }
        P::OpenaiCompat {
            base_url, auth, ..
        } => Plan::Http(HttpPlan {
            url: format!("{}/models", trim_base(base_url)),
            auth: AuthMode::Bearer,
            parse: ParseStyle::OpenAiData,
            env_var: env_var_of(auth),
        }),
        P::Vllm {
            base_url, auth, ..
        } => local_openai_plan(base_url.as_deref(), VLLM_BASE, auth),
        P::Mlx {
            base_url, auth, ..
        } => local_openai_plan(base_url.as_deref(), MLX_BASE, auth),
        P::Llamacpp {
            base_url, auth, ..
        } => local_openai_plan(base_url.as_deref(), LLAMACPP_BASE, auth),
        P::Bedrock { .. } => Plan::Skip {
            note: "bedrock — foundation-model list is an AWS SDK (SigV4) call, not queried here"
                .to_string(),
        },
        P::ClaudeCli { .. }
        | P::GeminiCli { .. }
        | P::ClaudeSdk { .. }
        | P::CodexCli { .. }
        | P::Opencode { .. }
        | P::Antigravity { .. } => Plan::Skip {
            note: "CLI provider — models via its own login; not queryable".to_string(),
        },
        P::Mock { .. } | P::Cassette { .. } => Plan::Skip {
            note: "internal test provider; not queryable".to_string(),
        },
    }
}

fn local_openai_plan(base_url: Option<&str>, default: &str, auth: &Auth) -> Plan {
    let base = trim_base(base_url.unwrap_or(default));
    Plan::Http(HttpPlan {
        url: format!("{base}/models"),
        auth: AuthMode::Bearer,
        parse: ParseStyle::OpenAiData,
        env_var: env_var_of(auth),
    })
}

/// Parse a provider's list-models response body into sorted, de-duplicated
/// model ids. Pure — the unit-tested seam.
pub fn parse_models(style: ParseStyle, body: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("response was not JSON: {e}"))?;
    let mut ids: Vec<String> = match style {
        ParseStyle::Gemini => v
            .get("models")
            .and_then(|m| m.as_array())
            .ok_or_else(|| "missing `models` array".to_string())?
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
            // `models/gemini-2.5-flash` → `gemini-2.5-flash`.
            .map(|n| n.strip_prefix("models/").unwrap_or(n).to_string())
            .collect(),
        ParseStyle::OpenAiData => v
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| "missing `data` array".to_string())?
            .iter()
            .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
            .map(str::to_string)
            .collect(),
    };
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Outcome of attempting to list one provider's models.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Live list retrieved.
    Ok { models: Vec<String> },
    /// Provider authenticates via an env var that is unset. Carries the var
    /// name so the user knows what to export. The key is never read here.
    NoKey { var: String },
    /// Not queryable (CLI / bedrock / mock / cassette), with the reason.
    Skipped { note: String },
    /// Server answered but rejected our credential (401/403).
    AuthFailed { status: u16 },
    /// Server answered with some other non-success status.
    HttpStatus { status: u16 },
    /// Could not reach the server (DNS/connect/timeout).
    Unreachable { detail: String },
    /// Reached + 2xx, but the body didn't parse as the expected shape.
    ParseError { detail: String },
}

/// Execute a [`Plan`] against the live API. Bounded by `timeout`; best-effort
/// and total (never panics). The API key, when present, is read from the
/// process env at call time and used only to build the request — it is never
/// returned, logged, or stored.
pub async fn query(client: &reqwest::Client, plan: &Plan, timeout: Duration) -> Outcome {
    let plan = match plan {
        Plan::Http(p) => p,
        Plan::Skip { note } => return Outcome::Skipped { note: note.clone() },
    };

    // Resolve the key (if this provider needs one).
    let key = match &plan.env_var {
        Some(var) => match std::env::var(var) {
            Ok(k) if !k.trim().is_empty() => Some(k),
            _ => return Outcome::NoKey { var: var.clone() },
        },
        None => None, // keyless local server
    };

    let mut req = client.get(&plan.url).timeout(timeout);
    match &plan.auth {
        AuthMode::Bearer => {
            if let Some(k) = &key {
                req = req.bearer_auth(k);
            }
        }
        AuthMode::XApiKey { version } => {
            if let Some(k) = &key {
                req = req.header("x-api-key", k);
            }
            req = req.header("anthropic-version", version);
        }
        AuthMode::GeminiQuery => {
            // Gemini requires the key; `plan.env_var` was Some or we'd have a
            // key. Passed as a query param, never stored in `plan.url`.
            if let Some(k) = &key {
                req = req.query(&[("key", k.as_str())]);
            }
        }
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return Outcome::Unreachable {
                // reqwest's Display omits the key; still, we only surface the
                // error's own text, which is URL/host/timeout — no credential.
                detail: reqwest_error_detail(&e),
            };
        }
    };

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Outcome::AuthFailed {
            status: status.as_u16(),
        };
    }
    if !status.is_success() {
        return Outcome::HttpStatus {
            status: status.as_u16(),
        };
    }

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            return Outcome::Unreachable {
                detail: reqwest_error_detail(&e),
            }
        }
    };

    match parse_models(plan.parse, &body) {
        Ok(models) => Outcome::Ok { models },
        Err(detail) => Outcome::ParseError { detail },
    }
}

/// Human-readable, key-free description of a reqwest error.
fn reqwest_error_detail(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "request timed out".to_string()
    } else if e.is_connect() {
        "could not connect (server down / wrong base_url?)".to_string()
    } else {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::HttpProviderExtras;

    // ── parse: Gemini (captured shape from generativelanguage v1beta) ──
    #[test]
    fn parse_gemini_strips_models_prefix_and_sorts() {
        let body = r#"{
          "models": [
            { "name": "models/gemini-2.5-flash", "displayName": "Flash" },
            { "name": "models/gemini-3.1-flash-lite" },
            { "name": "models/gemini-flash-lite-latest" }
          ]
        }"#;
        let got = parse_models(ParseStyle::Gemini, body).expect("parse");
        assert_eq!(
            got,
            vec![
                "gemini-2.5-flash",
                "gemini-3.1-flash-lite",
                "gemini-flash-lite-latest",
            ]
        );
    }

    // ── parse: OpenAI `{data:[{id}]}` (also DeepSeek / LM Studio) ──
    #[test]
    fn parse_openai_data_extracts_ids() {
        let body = r#"{
          "object": "list",
          "data": [
            { "id": "gpt-4o", "object": "model" },
            { "id": "gpt-4o-mini", "object": "model" }
          ]
        }"#;
        let got = parse_models(ParseStyle::OpenAiData, body).expect("parse");
        assert_eq!(got, vec!["gpt-4o", "gpt-4o-mini"]);
    }

    // ── parse: Anthropic `/v1/models` (same {data:[{id}]} shape) ──
    #[test]
    fn parse_anthropic_uses_data_id_shape() {
        let body = r#"{
          "data": [
            { "type": "model", "id": "claude-sonnet-4-5", "display_name": "Sonnet" },
            { "type": "model", "id": "claude-opus-4-1" }
          ],
          "has_more": false
        }"#;
        let got = parse_models(ParseStyle::OpenAiData, body).expect("parse");
        assert_eq!(got, vec!["claude-opus-4-1", "claude-sonnet-4-5"]);
    }

    #[test]
    fn parse_dedups_repeated_ids() {
        let body = r#"{"data":[{"id":"m"},{"id":"m"},{"id":"a"}]}"#;
        let got = parse_models(ParseStyle::OpenAiData, body).expect("parse");
        assert_eq!(got, vec!["a", "m"]);
    }

    #[test]
    fn parse_rejects_non_json() {
        let err = parse_models(ParseStyle::Gemini, "<html>502</html>").unwrap_err();
        assert!(err.contains("not JSON"), "got: {err}");
    }

    #[test]
    fn parse_rejects_wrong_shape() {
        // Gemini parser on an OpenAI body → missing `models`.
        let err = parse_models(ParseStyle::Gemini, r#"{"data":[]}"#).unwrap_err();
        assert!(err.contains("models"), "got: {err}");
    }

    // ── plan: each provider type maps to the right endpoint/auth ──
    #[test]
    fn plan_gemini_uses_v1beta_query_key() {
        let cfg = ProviderConfig::Gemini {
            base_url: None,
            auth: Auth::env("GEMINI_API_KEY"),
            default_model: "gemini-2.5-flash".into(),
            extras: HttpProviderExtras::default(),
        };
        match plan_for(&cfg) {
            Plan::Http(p) => {
                assert_eq!(
                    p.url,
                    "https://generativelanguage.googleapis.com/v1beta/models"
                );
                assert_eq!(p.auth, AuthMode::GeminiQuery);
                assert_eq!(p.parse, ParseStyle::Gemini);
                assert_eq!(p.env_var.as_deref(), Some("GEMINI_API_KEY"));
            }
            other => panic!("wrong plan: {other:?}"),
        }
    }

    #[test]
    fn plan_anthropic_sets_version_and_xapikey() {
        let cfg = ProviderConfig::Anthropic {
            base_url: None,
            api_version: None,
            auth: Auth::env("ANTHROPIC_API_KEY"),
            default_model: "claude-sonnet-4-5".into(),
            extras: HttpProviderExtras::default(),
        };
        match plan_for(&cfg) {
            Plan::Http(p) => {
                assert_eq!(p.url, "https://api.anthropic.com/v1/models");
                assert_eq!(
                    p.auth,
                    AuthMode::XApiKey {
                        version: "2023-06-01".into()
                    }
                );
                assert_eq!(p.parse, ParseStyle::OpenAiData);
            }
            other => panic!("wrong plan: {other:?}"),
        }
    }

    #[test]
    fn plan_openai_compat_uses_configured_base_and_is_keyless_when_auth_none() {
        let cfg = ProviderConfig::OpenaiCompat {
            base_url: "http://127.0.0.1:1234/v1".into(),
            auth: Auth::None,
            default_model: "qwen/qwen3-coder-30b".into(),
            extras: HttpProviderExtras::default(),
            capabilities: Default::default(),
        };
        match plan_for(&cfg) {
            Plan::Http(p) => {
                assert_eq!(p.url, "http://127.0.0.1:1234/v1/models");
                assert_eq!(p.auth, AuthMode::Bearer);
                assert!(p.env_var.is_none(), "keyless local → no env var");
            }
            other => panic!("wrong plan: {other:?}"),
        }
    }

    #[test]
    fn plan_mlx_falls_back_to_localhost_8080() {
        let cfg = ProviderConfig::Mlx {
            base_url: None,
            auth: Auth::None,
            default_model: "m".into(),
            extras: HttpProviderExtras::default(),
            capabilities: Default::default(),
        };
        match plan_for(&cfg) {
            Plan::Http(p) => assert_eq!(p.url, "http://localhost:8080/v1/models"),
            other => panic!("wrong plan: {other:?}"),
        }
    }

    #[test]
    fn plan_cli_and_bedrock_and_mock_are_skipped() {
        let cli = ProviderConfig::GeminiCli {
            executable: "gemini".into(),
            timeout_secs: 300,
            default_model: "gemini-3-flash-preview".into(),
        };
        assert!(matches!(plan_for(&cli), Plan::Skip { .. }));

        let bedrock = ProviderConfig::Bedrock {
            region: "us-east-1".into(),
            model: "m".into(),
            profile: None,
        };
        assert!(matches!(plan_for(&bedrock), Plan::Skip { .. }));

        let mock = ProviderConfig::Mock {
            canned_response: "hi".into(),
        };
        assert!(matches!(plan_for(&mock), Plan::Skip { .. }));
    }

    #[test]
    fn trailing_slash_on_base_url_is_normalised() {
        let cfg = ProviderConfig::OpenaiCompat {
            base_url: "http://x/v1/".into(),
            auth: Auth::None,
            default_model: "m".into(),
            extras: HttpProviderExtras::default(),
            capabilities: Default::default(),
        };
        match plan_for(&cfg) {
            Plan::Http(p) => assert_eq!(p.url, "http://x/v1/models"),
            other => panic!("wrong plan: {other:?}"),
        }
    }
}
