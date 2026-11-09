//! Lists a provider's models over its REST API, for `tars models` and `tars providers --check`.
//!
//! [`plan_for`] decides from the config alone which endpoint, auth placement and response shape
//! a provider has (CLI, mock and cassette have no list API), and [`parse_models`] reads
//! a response body; both are pure. [`query`] resolves the key, sends the one GET with a timeout
//! and classifies the outcome.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, InvalidHeaderValue};

use tars_config::ProviderConfig;
use tars_config::model_library::LiveModel;
use tars_llm::{
    AuthError, Endpoint, EnvFileAuthResolver, EnvSource, ResolvedAuth, Route, apply_extra_headers,
    route,
};
use tars_types::{Auth, HttpOverrides, SecretRef};

/// Shape of the provider's list-models JSON response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListShape {
    /// Gemini: `{ "models": [ { "name": "models/gemini-2.5-flash",
    /// "inputTokenLimit": …, "outputTokenLimit": … } ] }`. The `models/`
    /// prefix is stripped.
    Gemini,
    /// OpenAI / OpenAI-compatible / Anthropic: `{ "data": [ { "id": "…" } ] }`.
    OpenAiData,
}

/// Where the API key rides on the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyPlacement {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// `x-api-key: <key>` + `anthropic-version: <version>`.
    XApiKey { version: String },
    /// `x-goog-api-key: <key>`.
    GoogApiKey,
}

/// A concrete plan to list one provider's models over HTTP.
#[derive(Clone, Debug)]
pub struct HttpPlan<'a> {
    /// The URL; no auth mode puts the key in it.
    pub url: String,
    pub auth: KeyPlacement,
    pub parse: ListShape,
    /// The provider's `auth` setting, resolved into the key when the query is sent.
    pub credential: Auth,
    /// The headers and query parameters the provider's config adds to each of its requests,
    /// borrowed from that config.
    pub extras: &'a HttpOverrides,
    pub hosting: Hosting,
}

/// Who runs the server a plan asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hosting {
    /// The vendor runs it; a refused connection is a failure.
    Vendor,
    /// The operator runs it on their own machine (vllm, mlx, llama.cpp); a refused connection
    /// means it is not started.
    SelfHosted,
}

/// How a provider's models can be discovered.
#[derive(Clone, Debug)]
pub enum Plan<'a> {
    Http(HttpPlan<'a>),
    /// Not queryable (CLI provider, mock, cassette), with a reason to show verbatim.
    Unlisted {
        note: String,
    },
}

fn trim_base(base: &str) -> &str {
    base.trim_end_matches('/')
}

/// Decides how a provider's models can be listed, at the endpoint tars-llm would call.
pub fn plan_for(cfg: &ProviderConfig) -> Plan<'_> {
    let (endpoint, credential, extras) = match route(cfg) {
        Route::Http {
            endpoint,
            auth,
            extras,
        } => (endpoint, auth.clone(), extras),
        Route::Mock => return unlisted("internal test provider; not queryable"),
        Route::Cli | Route::Api => {
            return unlisted("CLI provider — models via its own login; not queryable");
        }
    };
    let hosting = if cfg.kind().self_hosted() {
        Hosting::SelfHosted
    } else {
        Hosting::Vendor
    };
    Plan::Http(match endpoint {
        Endpoint::Gemini { base_url } => HttpPlan {
            url: format!("{}/v1beta/models", trim_base(&base_url)),
            auth: KeyPlacement::GoogApiKey,
            parse: ListShape::Gemini,
            credential,
            extras,
            hosting,
        },
        Endpoint::Anthropic {
            base_url,
            api_version,
        } => HttpPlan {
            url: format!("{}/v1/models", trim_base(&base_url)),
            auth: KeyPlacement::XApiKey {
                version: api_version,
            },
            parse: ListShape::OpenAiData,
            credential,
            extras,
            hosting,
        },
        Endpoint::OpenAi { base_url } => HttpPlan {
            url: format!("{}/models", trim_base(&base_url)),
            auth: KeyPlacement::Bearer,
            parse: ListShape::OpenAiData,
            credential,
            extras,
            hosting,
        },
    })
}

fn unlisted(note: &str) -> Plan<'static> {
    Plan::Unlisted {
        note: note.to_string(),
    }
}

/// Parses a list-models response body into models sorted and de-duplicated by id, keeping
/// Gemini's token limits; OpenAI-shaped lists carry ids only.
///
/// # Errors
///
/// Returns the reason when the body is not JSON or lacks the expected array.
pub fn parse_models(style: ListShape, body: &str) -> Result<Vec<LiveModel>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("response was not JSON: {e}"))?;
    let mut models: Vec<LiveModel> = match style {
        ListShape::Gemini => v
            .get("models")
            .and_then(|m| m.as_array())
            .ok_or_else(|| "missing `models` array".to_string())?
            .iter()
            .filter_map(|m| {
                let name = m.get("name").and_then(|n| n.as_str())?;
                Some(LiveModel {
                    id: name.strip_prefix("models/").unwrap_or(name).to_string(),
                    context: m.get("inputTokenLimit").and_then(|n| n.as_u64()),
                    max_output: m.get("outputTokenLimit").and_then(|n| n.as_u64()),
                })
            })
            .collect(),
        ListShape::OpenAiData => v
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| "missing `data` array".to_string())?
            .iter()
            .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
            .map(LiveModel::id_only)
            .collect(),
    };
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

/// ListOutcome of attempting to list one provider's models.
#[derive(Clone, Debug)]
pub enum ListOutcome {
    Ok {
        models: Vec<LiveModel>,
    },
    /// The key's env var is unset or blank; carries its name so the user knows what to export.
    NoKey {
        var: String,
    },
    /// The config names a key that did not resolve (an unreadable key file, a blank inline key,
    /// a variable that is not UTF-8), said by tars-llm's resolver.
    KeyUnresolved {
        reason: String,
    },
    /// The key cannot be sent in `header`: it holds a byte a header value refuses (a newline).
    KeyInvalid {
        header: &'static str,
    },
    /// Server answered but rejected our credential (401/403).
    AuthFailed {
        status: u16,
        body: Body,
    },
    /// Server answered with some other non-success status, a redirect included.
    HttpStatus {
        status: u16,
        body: Body,
    },
    /// Could not reach the server (DNS/connect/timeout), or lost the connection reading a 2xx
    /// response.
    Unreachable {
        detail: String,
    },
    /// A self-hosted server refused the connection: it is not started, so nothing was asked.
    NotRunning {
        detail: String,
    },
    /// The server answered 2xx, but the body did not parse as the expected shape.
    ParseError {
        detail: String,
    },
}

/// The body of a non-2xx answer, as much of it as is worth showing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Its first [`BODY_EXCERPT_CHARS`] characters, control characters escaped.
    Read(String),
    /// Reading it failed, with reqwest's reason.
    Unreadable(String),
}

const BODY_EXCERPT_CHARS: usize = 300;

impl std::fmt::Display for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(text) => f.write_str(text),
            Self::Unreadable(reason) => write!(f, "body unreadable: {reason}"),
        }
    }
}

/// Returns `head` followed by the server's `body`, when it sent one.
pub fn with_body(head: String, body: &Body) -> String {
    match body {
        Body::Read(text) if text.is_empty() => head,
        body => format!("{head}: {body}"),
    }
}

/// Returns the start of `body` fit for one terminal line: whitespace runs collapsed, control
/// characters escaped (a body is the server's text), at most [`BODY_EXCERPT_CHARS`] characters.
fn excerpt(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat
        .chars()
        .take(BODY_EXCERPT_CHARS)
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect();
    if flat.chars().count() > BODY_EXCERPT_CHARS {
        out.push('…');
    }
    out
}

/// Says what a non-2xx status means: 401 and 403 refuse the credential, every other status
/// (a 3xx not followed included) is just that status.
fn refusal(status: u16, body: Body) -> ListOutcome {
    match status {
        401 | 403 => ListOutcome::AuthFailed { status, body },
        _ => ListOutcome::HttpStatus { status, body },
    }
}

/// Returns the client the model-list queries share. It follows no redirect: a list is one GET,
/// and a redirect would carry `x-api-key` or `x-goog-api-key` to whatever host it names, since
/// reqwest strips only `Authorization` when the host changes. A 3xx is an
/// [`ListOutcome::HttpStatus`].
///
/// # Errors
///
/// Fails when reqwest cannot build a client (no TLS backend).
pub fn client(connect_timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Runs an [`HttpPlan`] against the live API within `timeout`, its key and its `env_http_headers`
/// read from `env`. Every failure is an [`ListOutcome`], not an error. The key is used only to
/// build the request; it is never returned, logged, or stored.
pub async fn list_models(
    client: &reqwest::Client,
    plan: &HttpPlan<'_>,
    timeout: Duration,
    env: &Arc<dyn EnvSource>,
) -> ListOutcome {
    let resolver = EnvFileAuthResolver::new(env.clone());
    let key = match resolver.resolve_untenanted(&plan.credential).await {
        Ok(ResolvedAuth::ApiKey(k) | ResolvedAuth::Bearer(k)) => Some(k),
        Ok(ResolvedAuth::None) => None,
        Err(e) => return unresolved(&plan.credential, &e),
    };
    send(client, plan, key.as_deref(), env.as_ref(), timeout).await
}

/// Returns the outcome of a key that did not resolve: an env var that is not set or blank is
/// named so the user knows what to export; every other failure carries the resolver's reason.
fn unresolved(credential: &Auth, error: &AuthError) -> ListOutcome {
    match (credential, error) {
        (
            Auth::Secret {
                secret: SecretRef::Env { var },
            },
            AuthError::EnvUnset { .. } | AuthError::Empty { .. },
        ) => ListOutcome::NoKey { var: var.clone() },
        _ => ListOutcome::KeyUnresolved {
            reason: tars_utils::display_chain(error),
        },
    }
}

/// Sends the plan's request with `key` in the place its auth mode names, the config's extra
/// headers over them and its `query_params` after the URL's own, and reads the answer.
async fn send(
    client: &reqwest::Client,
    plan: &HttpPlan<'_>,
    key: Option<&str>,
    env: &dyn EnvSource,
    timeout: Duration,
) -> ListOutcome {
    let mut req = client.get(&plan.url).timeout(timeout);
    if let KeyPlacement::XApiKey { version } = &plan.auth {
        req = req.header("anthropic-version", version);
    }
    if let Some(k) = key {
        match credential_header(&plan.auth, k) {
            Ok((name, value)) => req = req.header(name, value),
            Err(header) => return ListOutcome::KeyInvalid { header },
        }
    }
    let mut extra = HeaderMap::new();
    apply_extra_headers(plan.extras, env, &mut extra);
    req = req.headers(extra);

    let sent = match req.build() {
        Ok(mut request) => {
            plan.extras.apply_query_params(request.url_mut());
            client.execute(request).await
        }
        Err(e) => Err(e),
    };
    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            let connect_failed = e.is_connect();
            return unsent(plan.hosting, connect_failed, reqwest_error_detail(e));
        }
    };

    let status = resp.status();
    let body = resp.text().await;
    if !status.is_success() {
        let body = match body {
            Ok(text) => Body::Read(excerpt(&text)),
            Err(e) => Body::Unreadable(reqwest_error_detail(e)),
        };
        return refusal(status.as_u16(), body);
    }
    let body = match body {
        Ok(b) => b,
        Err(e) => {
            return ListOutcome::Unreachable {
                detail: reqwest_error_detail(e),
            };
        }
    };

    match parse_models(plan.parse, &body) {
        Ok(models) => ListOutcome::Ok { models },
        Err(detail) => ListOutcome::ParseError { detail },
    }
}

/// Says what a request that never got an answer means: a self-hosted server that refused the
/// connection is not running; any other failure to reach a server is a failure.
fn unsent(hosting: Hosting, connect_failed: bool, detail: String) -> ListOutcome {
    match (hosting, connect_failed) {
        (Hosting::SelfHosted, true) => ListOutcome::NotRunning { detail },
        (Hosting::SelfHosted, false) | (Hosting::Vendor, _) => ListOutcome::Unreachable { detail },
    }
}

const GEMINI_API_KEY_HEADER: &str = "x-goog-api-key";

/// Returns the header `key` rides in under `auth`, its value marked sensitive.
///
/// # Errors
///
/// Returns the header's name when `key` is not a valid header value.
fn credential_header(
    auth: &KeyPlacement,
    key: &str,
) -> Result<(&'static str, HeaderValue), &'static str> {
    let (name, value) = match auth {
        KeyPlacement::Bearer => ("authorization", format!("Bearer {key}")),
        KeyPlacement::XApiKey { .. } => ("x-api-key", key.to_string()),
        KeyPlacement::GoogApiKey => (GEMINI_API_KEY_HEADER, key.to_string()),
    };
    sensitive_header_value(&value)
        .map(|v| (name, v))
        .map_err(|_| name)
}

/// Returns `key` as a header value marked sensitive, so a `HeaderMap`'s `Debug` does not print it.
fn sensitive_header_value(key: &str) -> Result<HeaderValue, InvalidHeaderValue> {
    let mut v = HeaderValue::from_str(key)?;
    v.set_sensitive(true);
    Ok(v)
}

/// Describes a reqwest error with its URL and whole cause chain; the text lands in `models.json`
/// and on stdout.
fn reqwest_error_detail(e: reqwest::Error) -> String {
    let hint = if e.is_timeout() {
        Some("request timed out")
    } else if e.is_connect() {
        Some("could not connect (server down or wrong base_url?)")
    } else {
        None
    };
    let mut detail = hint.map_or_else(|| e.to_string(), |h| format!("{h}: {e}"));
    let mut cause = std::error::Error::source(&e);
    while let Some(c) = cause {
        detail.push_str(": ");
        detail.push_str(&c.to_string());
        cause = c.source();
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_llm::ProcessEnv;

    static NO_EXTRAS: std::sync::LazyLock<HttpOverrides> =
        std::sync::LazyLock::new(HttpOverrides::default);

    // ── parse: Gemini (captured shape from generativelanguage v1beta) ──
    #[test]
    fn parse_gemini_strips_models_prefix_sorts_and_keeps_limits() {
        let body = r#"{
          "models": [
            { "name": "models/gemini-3.8-flash", "displayName": "Flash",
              "inputTokenLimit": 1048576, "outputTokenLimit": 65536 },
            { "name": "models/gemini-3.1-flash-lite" },
            { "name": "models/gemini-flash-lite-latest" }
          ]
        }"#;
        let got = parse_models(ListShape::Gemini, body).expect("parse");
        assert_eq!(
            got,
            vec![
                LiveModel::id_only("gemini-3.1-flash-lite"),
                LiveModel {
                    id: "gemini-3.8-flash".into(),
                    context: Some(1_048_576),
                    max_output: Some(65_536),
                },
                LiveModel::id_only("gemini-flash-lite-latest"),
            ]
        );
    }

    fn ids(models: Vec<LiveModel>) -> Vec<String> {
        models.into_iter().map(|m| m.id).collect()
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
        let got = parse_models(ListShape::OpenAiData, body).expect("parse");
        assert_eq!(ids(got), vec!["gpt-4o", "gpt-4o-mini"]);
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
        let got = parse_models(ListShape::OpenAiData, body).expect("parse");
        assert_eq!(ids(got), vec!["claude-opus-4-1", "claude-sonnet-4-5"]);
    }

    #[test]
    fn parse_dedups_repeated_ids() {
        let body = r#"{"data":[{"id":"m"},{"id":"m"},{"id":"a"}]}"#;
        let got = parse_models(ListShape::OpenAiData, body).expect("parse");
        assert_eq!(ids(got), vec!["a", "m"]);
    }

    #[test]
    fn parse_rejects_non_json() {
        let err = parse_models(ListShape::Gemini, "<html>502</html>").unwrap_err();
        assert!(err.contains("not JSON"), "got: {err}");
    }

    #[test]
    fn parse_rejects_wrong_shape() {
        // Gemini parser on an OpenAI body → missing `models`.
        let err = parse_models(ListShape::Gemini, r#"{"data":[]}"#).unwrap_err();
        assert!(err.contains("models"), "got: {err}");
    }

    // ── plan: each provider type maps to the right endpoint/auth ──
    #[test]
    fn plan_gemini_uses_v1beta_and_the_goog_api_key_header() {
        let cfg = ProviderConfig::Gemini {
            base_url: None,
            auth: Auth::env("GEMINI_API_KEY"),
            default_model: "gemini-2.5-flash".into(),
            extras: HttpOverrides::default(),
        };
        match plan_for(&cfg) {
            Plan::Http(p) => {
                assert_eq!(
                    p.url,
                    "https://generativelanguage.googleapis.com/v1beta/models"
                );
                assert_eq!(p.auth, KeyPlacement::GoogApiKey);
                assert_eq!(p.parse, ListShape::Gemini);
                assert!(
                    matches!(&p.credential, Auth::Secret { secret: SecretRef::Env { var } } if var == "GEMINI_API_KEY")
                );
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
            extras: HttpOverrides::default(),
        };
        match plan_for(&cfg) {
            Plan::Http(p) => {
                assert_eq!(p.url, "https://api.anthropic.com/v1/models");
                assert_eq!(
                    p.auth,
                    KeyPlacement::XApiKey {
                        version: "2023-06-01".into()
                    }
                );
                assert_eq!(p.parse, ListShape::OpenAiData);
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
            gen_ai_provider_name: "local".into(),
            extras: HttpOverrides::default(),
            capabilities: Default::default(),
        };
        match plan_for(&cfg) {
            Plan::Http(p) => {
                assert_eq!(p.url, "http://127.0.0.1:1234/v1/models");
                assert_eq!(p.auth, KeyPlacement::Bearer);
                assert!(
                    matches!(p.credential, Auth::None),
                    "keyless local → no credential"
                );
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
            extras: HttpOverrides::default(),
            capabilities: Default::default(),
        };
        match plan_for(&cfg) {
            Plan::Http(p) => assert_eq!(p.url, "http://localhost:8080/v1/models"),
            other => panic!("wrong plan: {other:?}"),
        }
    }

    #[test]
    fn a_local_server_plan_is_self_hosted_and_a_vendor_plan_is_not() {
        let local = ProviderConfig::Llamacpp {
            base_url: None,
            auth: Auth::None,
            default_model: "m".into(),
            extras: HttpOverrides::default(),
            capabilities: Default::default(),
        };
        let vendor = ProviderConfig::Gemini {
            base_url: None,
            auth: Auth::env("GEMINI_API_KEY"),
            default_model: "gemini-2.5-flash".into(),
            extras: HttpOverrides::default(),
        };
        for (cfg, hosting) in [(local, Hosting::SelfHosted), (vendor, Hosting::Vendor)] {
            match plan_for(&cfg) {
                Plan::Http(p) => assert_eq!(p.hosting, hosting),
                other => panic!("wrong plan: {other:?}"),
            }
        }
    }

    #[test]
    fn plan_cli_and_mock_are_skipped() {
        let cli = ProviderConfig::Antigravity {
            executable: "agy".into(),
            timeout_secs: 300,
            default_model: "gemini-3.1-pro".into(),
            effort: tars_config::AntigravityEffortConfig::High,
        };
        assert!(matches!(plan_for(&cli), Plan::Unlisted { .. }));

        let mock = ProviderConfig::Mock {
            canned_response: "hi".into(),
        };
        assert!(matches!(plan_for(&mock), Plan::Unlisted { .. }));
    }

    #[test]
    fn trailing_slash_on_base_url_is_normalised() {
        let cfg = ProviderConfig::OpenaiCompat {
            base_url: "http://x/v1/".into(),
            auth: Auth::None,
            default_model: "m".into(),
            gen_ai_provider_name: "local".into(),
            extras: HttpOverrides::default(),
            capabilities: Default::default(),
        };
        match plan_for(&cfg) {
            Plan::Http(p) => assert_eq!(p.url, "http://x/v1/models"),
            other => panic!("wrong plan: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unreachable_error_keeps_its_cause_and_the_url_it_asked() {
        let err = reqwest::Client::new()
            .get("http://127.0.0.1:1/v1beta/models")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        let detail = reqwest_error_detail(err);
        assert!(detail.starts_with("could not connect"), "{detail}");
        assert!(
            detail.contains("http://127.0.0.1:1/v1beta/models"),
            "{detail}"
        );
    }

    const SECRET_KEY: &str = "AIzaSEKRETKEY123";

    fn gemini_plan(url: String) -> HttpPlan<'static> {
        HttpPlan {
            url,
            auth: KeyPlacement::GoogApiKey,
            parse: ListShape::Gemini,
            credential: Auth::env("GEMINI_API_KEY"),
            extras: &NO_EXTRAS,
            hosting: Hosting::Vendor,
        }
    }

    #[test]
    fn a_key_becomes_a_sensitive_header_value_and_one_with_a_newline_is_refused() {
        let v = sensitive_header_value("k").unwrap();
        assert_eq!(v, "k");
        assert!(v.is_sensitive());

        let err = sensitive_header_value("AIza\nSECRET").unwrap_err();
        assert!(!format!("{err} {err:?}").contains("SECRET"));
    }

    #[tokio::test]
    async fn a_failed_gemini_listing_names_its_url_but_not_its_key() {
        let url = "http://127.0.0.1:1/v1beta/models".to_string();
        let outcome = send(
            &reqwest::Client::new(),
            &gemini_plan(url.clone()),
            Some(SECRET_KEY),
            &ProcessEnv,
            Duration::from_secs(5),
        )
        .await;
        match outcome {
            ListOutcome::Unreachable { detail } => {
                assert!(detail.contains(&url), "{detail}");
                assert!(!detail.contains(SECRET_KEY), "{detail}");
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_self_hosted_server_that_refuses_the_connection_is_not_running() {
        let plan = HttpPlan {
            url: "http://127.0.0.1:1/v1/models".into(),
            auth: KeyPlacement::Bearer,
            parse: ListShape::OpenAiData,
            credential: Auth::None,
            extras: &NO_EXTRAS,
            hosting: Hosting::SelfHosted,
        };
        let outcome = send(
            &reqwest::Client::new(),
            &plan,
            None,
            &ProcessEnv,
            Duration::from_secs(5),
        )
        .await;
        match outcome {
            ListOutcome::NotRunning { detail } => {
                assert!(detail.starts_with("could not connect"), "{detail}");
            }
            other => panic!("expected NotRunning, got {other:?}"),
        }
    }

    #[test]
    fn only_a_refused_connection_to_a_self_hosted_server_means_it_is_not_running() {
        let d = || "x".to_string();
        assert!(matches!(
            unsent(Hosting::SelfHosted, true, d()),
            ListOutcome::NotRunning { .. }
        ));
        assert!(matches!(
            unsent(Hosting::SelfHosted, false, d()),
            ListOutcome::Unreachable { .. }
        ));
        assert!(matches!(
            unsent(Hosting::Vendor, true, d()),
            ListOutcome::Unreachable { .. }
        ));
    }

    #[tokio::test]
    async fn a_gemini_listing_sends_the_key_in_the_x_goog_api_key_header_and_not_in_the_url() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1beta/models"))
            .and(header(GEMINI_API_KEY_HEADER, SECRET_KEY))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"models":[]}"#))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = send(
            &reqwest::Client::new(),
            &gemini_plan(format!("{}/v1beta/models", server.uri())),
            Some(SECRET_KEY),
            &ProcessEnv,
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(outcome, ListOutcome::Ok { .. }), "{outcome:?}");

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        assert!(
            !received[0].url.as_str().contains(SECRET_KEY),
            "{}",
            received[0].url
        );
    }

    #[tokio::test]
    async fn a_key_in_a_file_is_resolved_and_sent() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1beta/models"))
            .and(header(GEMINI_API_KEY_HEADER, SECRET_KEY))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"models":[]}"#))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("key");
        std::fs::write(&key_file, format!("{SECRET_KEY}\n")).unwrap();
        let plan = HttpPlan {
            credential: Auth::file(&key_file),
            ..gemini_plan(format!("{}/v1beta/models", server.uri()))
        };

        let outcome = list_models(
            &reqwest::Client::new(),
            &plan,
            Duration::from_secs(5),
            &(Arc::new(ProcessEnv) as Arc<dyn EnvSource>),
        )
        .await;
        assert!(matches!(outcome, ListOutcome::Ok { .. }), "{outcome:?}");
    }

    struct KeyAndOrg;

    impl EnvSource for KeyAndOrg {
        fn var(&self, name: &str) -> Result<String, std::env::VarError> {
            match name {
                "TEST_KEY" => Ok("sk-env".into()),
                "TEST_ORG" => Ok("org-1".into()),
                _ => Err(std::env::VarError::NotPresent),
            }
        }
    }

    #[tokio::test]
    async fn a_model_listing_carries_the_configured_static_and_env_headers() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer sk-env"))
            .and(header("x-static", "s-1"))
            .and(header("openai-organization", "org-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
            .expect(1)
            .mount(&server)
            .await;
        let plan = HttpPlan {
            url: format!("{}/v1/models", server.uri()),
            auth: KeyPlacement::Bearer,
            parse: ListShape::OpenAiData,
            credential: Auth::env("TEST_KEY"),
            extras: &HttpOverrides {
                http_headers: [("X-Static".into(), "s-1".into())].into_iter().collect(),
                env_http_headers: [("OpenAI-Organization".into(), "TEST_ORG".into())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            hosting: Hosting::Vendor,
        };

        let env: Arc<dyn EnvSource> = Arc::new(KeyAndOrg);
        let outcome = list_models(
            &client(Duration::from_secs(5)).unwrap(),
            &plan,
            Duration::from_secs(5),
            &env,
        )
        .await;
        assert!(matches!(outcome, ListOutcome::Ok { .. }), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_model_listing_carries_the_configured_query_params_after_the_urls_own() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
            .expect(1)
            .mount(&server)
            .await;
        let plan = HttpPlan {
            url: format!("{}/v1/models?limit=5", server.uri()),
            auth: KeyPlacement::Bearer,
            parse: ListShape::OpenAiData,
            credential: Auth::None,
            extras: &HttpOverrides {
                query_params: [("z".into(), "1".into()), ("a".into(), "2".into())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            hosting: Hosting::Vendor,
        };

        let env: Arc<dyn EnvSource> = Arc::new(KeyAndOrg);
        let outcome = list_models(
            &client(Duration::from_secs(5)).unwrap(),
            &plan,
            Duration::from_secs(5),
            &env,
        )
        .await;
        assert!(matches!(outcome, ListOutcome::Ok { .. }), "{outcome:?}");
        let received = server.received_requests().await.unwrap();
        assert_eq!(received[0].url.query(), Some("limit=5&a=2&z=1"));
    }

    #[test]
    fn a_planned_listing_takes_the_providers_extras() {
        let cfg = ProviderConfig::Openai {
            base_url: None,
            auth: Auth::None,
            default_model: "m".into(),
            extras: HttpOverrides {
                http_headers: [("X-Static".into(), "s-1".into())].into_iter().collect(),
                ..Default::default()
            },
        };
        match plan_for(&cfg) {
            Plan::Http(p) => assert_eq!(p.extras.http_headers["X-Static"], "s-1"),
            other => panic!("wrong plan: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_redirect_to_another_host_is_not_followed_with_the_key() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
            .expect(0)
            .mount(&elsewhere)
            .await;
        let gateway = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/v1/models", elsewhere.uri())),
            )
            .mount(&gateway)
            .await;
        let plan = HttpPlan {
            url: format!("{}/v1/models", gateway.uri()),
            auth: KeyPlacement::XApiKey {
                version: "2023-06-01".into(),
            },
            parse: ListShape::OpenAiData,
            credential: Auth::env("ANTHROPIC_API_KEY"),
            extras: &NO_EXTRAS,
            hosting: Hosting::Vendor,
        };

        let outcome = send(
            &client(Duration::from_secs(5)).unwrap(),
            &plan,
            Some(SECRET_KEY),
            &ProcessEnv,
            Duration::from_secs(5),
        )
        .await;
        assert!(
            matches!(outcome, ListOutcome::HttpStatus { status: 302, .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn an_unset_env_key_is_named_and_any_other_failure_keeps_its_reason() {
        let missing = AuthError::EnvUnset { var: "K".into() };
        assert!(matches!(
            unresolved(&Auth::env("K"), &missing),
            ListOutcome::NoKey { var } if var == "K"
        ));
        let unreadable = AuthError::Open {
            path: "/k".into(),
            source: std::io::Error::other("denied"),
        };
        assert!(matches!(
            unresolved(&Auth::file("/k"), &unreadable),
            ListOutcome::KeyUnresolved { reason } if reason.contains("`/k`: denied")
        ));
    }

    #[test]
    fn a_key_that_is_no_header_value_is_invalid_in_every_auth_mode() {
        let modes = [
            KeyPlacement::Bearer,
            KeyPlacement::XApiKey {
                version: "2023-06-01".into(),
            },
            KeyPlacement::GoogApiKey,
        ];
        for mode in &modes {
            assert!(credential_header(mode, "sk-ok").is_ok(), "{mode:?}");
            assert!(credential_header(mode, "sk\nbad").is_err(), "{mode:?}");
        }
        let (name, value) = credential_header(&KeyPlacement::Bearer, "sk-ok").unwrap();
        assert_eq!(
            (name, value.to_str().unwrap()),
            ("authorization", "Bearer sk-ok")
        );
        assert!(value.is_sensitive());
    }

    #[tokio::test]
    async fn a_key_with_a_newline_is_refused_before_anything_is_sent() {
        let outcome = send(
            &reqwest::Client::new(),
            &gemini_plan("http://127.0.0.1:1/v1beta/models".into()),
            Some("AIza\nSECRET"),
            &ProcessEnv,
            Duration::from_secs(5),
        )
        .await;
        assert!(
            matches!(
                outcome,
                ListOutcome::KeyInvalid {
                    header: GEMINI_API_KEY_HEADER
                }
            ),
            "{outcome:?}"
        );
    }

    #[test]
    fn only_401_and_403_refuse_the_credential() {
        let body = || Body::Read(String::new());
        for status in [401, 403] {
            assert!(matches!(
                refusal(status, body()),
                ListOutcome::AuthFailed { .. }
            ));
        }
        for status in [302, 400, 404, 500] {
            assert!(matches!(
                refusal(status, body()),
                ListOutcome::HttpStatus { .. }
            ));
        }
    }

    #[test]
    fn an_excerpt_is_one_line_of_escaped_text_cut_at_its_limit() {
        assert_eq!(excerpt("  quota\n exceeded  "), "quota exceeded");
        assert_eq!(excerpt("a\u{1b}[31mb"), "a\\u{1b}[31mb");
        let long = "x".repeat(BODY_EXCERPT_CHARS + 1);
        let cut = excerpt(&long);
        assert_eq!(cut.chars().count(), BODY_EXCERPT_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(
            excerpt(&"x".repeat(BODY_EXCERPT_CHARS)),
            "x".repeat(BODY_EXCERPT_CHARS)
        );
    }

    #[tokio::test]
    async fn a_refusal_carries_the_body_the_server_sent() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string("model list is down"))
            .mount(&server)
            .await;
        let outcome = send(
            &reqwest::Client::new(),
            &gemini_plan(format!("{}/v1beta/models", server.uri())),
            None,
            &ProcessEnv,
            Duration::from_secs(5),
        )
        .await;
        match outcome {
            ListOutcome::HttpStatus { status, body } => {
                assert_eq!(status, 503);
                assert_eq!(body, Body::Read("model list is down".into()));
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }
}
