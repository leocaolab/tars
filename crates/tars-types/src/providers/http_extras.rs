//! Static HTTP extras a provider may declare in config and apply at request build time.
//!
//! Three fields — `http_headers / env_http_headers / query_params` —
//! that let users declaratively customise providers without code
//! changes:
//!
//! ```toml
//! [providers.openai_main]
//! type = "openai"
//! auth = { kind = "secret", secret = { source = "env", var = "OPENAI_API_KEY" } }
//! default_model = "gpt-4o"
//! http_headers = { "X-Project-Hint" = "internal" }
//! env_http_headers = { "OpenAI-Organization" = "OPENAI_ORG", "OpenAI-Project" = "OPENAI_PROJECT" }
//! query_params = { "experimental_param" = "1" }
//! ```
//!
//! - **`http_headers`** is set verbatim at adapter build time.
//! - **`env_http_headers`** is resolved at request build time so a
//!   process that exports the env var after start sees the new value.
//!   Empty/missing env vars produce no header.
//! - **`query_params`** is appended to the adapter's URL in stable
//!   (sorted-by-key) order so URL fingerprints stay reproducible.

use std::collections::HashMap;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpProviderExtras {
    /// Static headers attached verbatim.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub http_headers: HashMap<String, String>,

    /// Map of `header_name → env_var_name`. Resolved at request time.
    /// Header is omitted if the env var is unset, empty, or whitespace-only.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env_http_headers: HashMap<String, String>,

    /// Static query params appended to the adapter's URL in sorted order.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub query_params: HashMap<String, String>,
}

impl HttpProviderExtras {
    pub fn is_empty(&self) -> bool {
        self.http_headers.is_empty()
            && self.env_http_headers.is_empty()
            && self.query_params.is_empty()
    }

    /// Append both static and env-resolved headers to `target`.
    ///
    /// Audit `tars-types-src-http-extras-1`: invalid header names /
    /// values used to be silently skipped (a typo in `http_headers`
    /// would produce zero-effect, no signal). They still don't bring
    /// down the request — failing here would block every call — but
    /// each failure now emits a `tracing::warn!` so the operator
    /// learns about misconfiguration instead of debugging "why isn't
    /// my X-Project-Hint header showing up upstream".
    pub fn apply_headers(&self, target: &mut HeaderMap) {
        for (k, v) in &self.http_headers {
            match (HeaderName::try_from(k), HeaderValue::try_from(v)) {
                (Ok(name), Ok(value)) => {
                    target.insert(name, value);
                }
                (Err(e), _) => {
                    tracing::warn!(
                        header_name = %k,
                        error = %e,
                        "http_extras: skipping header — invalid name",
                    );
                }
                (_, Err(e)) => {
                    tracing::warn!(
                        header_name = %k,
                        error = %e,
                        "http_extras: skipping header — invalid value",
                    );
                }
            }
        }
        for (header, env_var) in &self.env_http_headers {
            let val = match std::env::var(env_var) {
                Ok(v) if !v.trim().is_empty() => v,
                _ => continue, // env unset / empty / non-UTF8 → skip silently (expected)
            };
            match (HeaderName::try_from(header), HeaderValue::try_from(&val)) {
                (Ok(name), Ok(value)) => {
                    target.insert(name, value);
                }
                (Err(e), _) => {
                    tracing::warn!(
                        header_name = %header,
                        env_var = %env_var,
                        error = %e,
                        "http_extras: skipping env header — invalid name",
                    );
                }
                (_, Err(e)) => {
                    tracing::warn!(
                        header_name = %header,
                        env_var = %env_var,
                        error = %e,
                        "http_extras: skipping env header — invalid value",
                    );
                }
            }
        }
    }

    /// Append `query_params` to `url` in stable (sorted by key) order.
    pub fn apply_query_params(&self, url: &mut Url) {
        if self.query_params.is_empty() {
            return;
        }
        let mut sorted: Vec<(&String, &String)> = self.query_params.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let mut pairs = url.query_pairs_mut();
        for (k, v) in sorted {
            pairs.append_pair(k, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_default() {
        let e = HttpProviderExtras::default();
        assert!(e.is_empty());
    }

    #[test]
    fn applies_static_headers() {
        let extras = HttpProviderExtras {
            http_headers: [("X-Custom".into(), "v1".into())].into_iter().collect(),
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        extras.apply_headers(&mut headers);
        assert_eq!(headers.get("X-Custom").unwrap(), "v1");
    }

    #[test]
    fn skips_env_header_when_var_missing() {
        let extras = HttpProviderExtras {
            env_http_headers: [(
                "X-Org".into(),
                "TARS_HTTP_TEST_VAR_THAT_NEVER_EXISTS_99".into(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        extras.apply_headers(&mut headers);
        assert!(headers.get("X-Org").is_none());
    }

    #[test]
    fn applies_query_params_in_sorted_order() {
        let extras = HttpProviderExtras {
            query_params: [
                ("z".into(), "1".into()),
                ("a".into(), "2".into()),
                ("m".into(), "3".into()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let mut url = Url::parse("https://example.com/").unwrap();
        extras.apply_query_params(&mut url);
        assert_eq!(url.query(), Some("a=2&m=3&z=1"));
    }

    #[test]
    fn deserialise_from_toml() {
        let toml_str = r#"
            http_headers = { "X-A" = "1" }
            env_http_headers = { "X-B" = "FOO" }
            query_params = { "k" = "v" }
        "#;
        let e: HttpProviderExtras = toml::from_str(toml_str).unwrap();
        assert_eq!(e.http_headers.len(), 1);
        assert_eq!(e.env_http_headers.len(), 1);
        assert_eq!(e.query_params.len(), 1);
    }

    #[test]
    fn rejects_unknown_field() {
        let toml_str = r#"
            http_headers = { "X-A" = "1" }
            random_typo = { "X-B" = "FOO" }
        "#;
        let r: Result<HttpProviderExtras, _> = toml::from_str(toml_str);
        assert!(r.is_err());
    }
}
