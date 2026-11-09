//! Holds the extra headers and query parameters a provider's config declares, applied to every
//! request the provider builds:
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
//! It only describes the headers: the HTTP transport inserts them, looking each `env_http_headers`
//! var up on every request in the environment the provider was built with.
//! `query_params` are appended to every request's URL sorted by key, so URL fingerprints are
//! reproducible.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpOverrides {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub http_headers: HashMap<String, String>,

    /// Header name to the name of the env var that holds the header's value.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env_http_headers: HashMap<String, String>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub query_params: HashMap<String, String>,
}

impl HttpOverrides {
    pub fn is_empty(&self) -> bool {
        self.http_headers.is_empty()
            && self.env_http_headers.is_empty()
            && self.query_params.is_empty()
    }

    /// Appends `query_params` to `url`, sorted by key. Pairs already in `url` stay first and are
    /// never replaced: a configured key that `url` already carries is sent twice, and which one
    /// counts is the server's choice.
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
        let e = HttpOverrides::default();
        assert!(e.is_empty());
    }

    #[test]
    fn applies_query_params_in_sorted_order() {
        let extras = HttpOverrides {
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
    fn configured_params_follow_the_urls_own_and_replace_none_of_them() {
        let extras = HttpOverrides {
            query_params: [("key".into(), "cfg".into()), ("b".into(), "2".into())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let mut url = Url::parse("https://example.com/models?key=own&page=3").unwrap();
        extras.apply_query_params(&mut url);
        assert_eq!(url.query(), Some("key=own&page=3&b=2&key=cfg"));
    }

    #[test]
    fn deserialise_from_toml() {
        let toml_str = r#"
            http_headers = { "X-A" = "1" }
            env_http_headers = { "X-B" = "FOO" }
            query_params = { "k" = "v" }
        "#;
        let e: HttpOverrides = toml::from_str(toml_str).unwrap();
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
        let r: Result<HttpOverrides, _> = toml::from_str(toml_str);
        assert!(r.is_err());
    }
}
