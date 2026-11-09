//! Says where each HTTP provider of a config sends its requests, with the backends' default
//! addresses filled in, so a caller outside the registry reaches the host the runtime would.

use tars_config::ProviderConfig;
use tars_types::{Auth, HttpOverrides};

use crate::provider::backends::{anthropic, gemini, llamacpp, mlx, openai, vllm};

/// The API an HTTP provider speaks and the address it speaks it at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// OpenAI's API, or a server compatible with it.
    OpenAi {
        base_url: String,
    },
    Anthropic {
        base_url: String,
        api_version: String,
    },
    Gemini {
        base_url: String,
    },
}

/// How tars reaches a provider, one variant per [`tars_types::InterfaceKind`]; only an HTTP
/// provider has an endpoint, a credential of tars's own and configured extra headers to send to
/// it.
#[derive(Clone, Debug)]
pub enum Route<'a> {
    Http {
        endpoint: Endpoint,
        auth: &'a Auth,
        extras: &'a HttpOverrides,
    },
    /// A vendor binary that logs in on its own.
    Cli,
    /// The SDK daemon, which logs in on its own.
    Api,
    /// A mock or a cassette; no call is placed.
    Mock,
}

/// Returns how `cfg` is reached; for an HTTP provider, where it sends its requests.
pub fn route(cfg: &ProviderConfig) -> Route<'_> {
    use ProviderConfig as P;
    let or = |base_url: &Option<String>, default: &str| {
        base_url.clone().unwrap_or_else(|| default.to_string())
    };
    let http = |endpoint, auth, extras| Route::Http {
        endpoint,
        auth,
        extras,
    };
    match cfg {
        P::Openai {
            base_url,
            auth,
            extras,
            ..
        } => http(
            Endpoint::OpenAi {
                base_url: or(base_url, openai::DEFAULT_BASE_URL),
            },
            auth,
            extras,
        ),
        P::OpenaiCompat {
            base_url,
            auth,
            extras,
            ..
        } => http(
            Endpoint::OpenAi {
                base_url: base_url.clone(),
            },
            auth,
            extras,
        ),
        P::Anthropic {
            base_url,
            api_version,
            auth,
            extras,
            ..
        } => http(
            Endpoint::Anthropic {
                base_url: or(base_url, anthropic::DEFAULT_BASE_URL),
                api_version: or(api_version, anthropic::DEFAULT_API_VERSION),
            },
            auth,
            extras,
        ),
        P::Gemini {
            base_url,
            auth,
            extras,
            ..
        } => http(
            Endpoint::Gemini {
                base_url: or(base_url, gemini::DEFAULT_BASE_URL),
            },
            auth,
            extras,
        ),
        P::Vllm {
            base_url,
            auth,
            extras,
            ..
        } => http(
            Endpoint::OpenAi {
                base_url: vllm::normalize_base_url(base_url.clone()),
            },
            auth,
            extras,
        ),
        P::Mlx {
            base_url,
            auth,
            extras,
            ..
        } => http(
            Endpoint::OpenAi {
                base_url: or(base_url, mlx::DEFAULT_BASE_URL),
            },
            auth,
            extras,
        ),
        P::Llamacpp {
            base_url,
            auth,
            extras,
            ..
        } => http(
            Endpoint::OpenAi {
                base_url: or(base_url, llamacpp::DEFAULT_BASE_URL),
            },
            auth,
            extras,
        ),
        P::ClaudeCli { .. } | P::CodexCli { .. } | P::Opencode { .. } | P::Antigravity { .. } => {
            Route::Cli
        }
        P::ClaudeSdk { .. } => Route::Api,
        P::Mock { .. } | P::Cassette { .. } => Route::Mock,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_without_a_base_url_gets_its_backends_default() {
        let endpoint = |cfg| match route(cfg) {
            Route::Http { endpoint, .. } => Some(endpoint),
            Route::Cli | Route::Api | Route::Mock => None,
        };
        let official_gemini = tars_config::default_gemini();
        assert_eq!(
            endpoint(&official_gemini),
            Some(Endpoint::Gemini {
                base_url: gemini::DEFAULT_BASE_URL.to_string()
            })
        );
        let official_anthropic = tars_config::default_anthropic();
        assert_eq!(
            endpoint(&official_anthropic),
            Some(Endpoint::Anthropic {
                base_url: anthropic::DEFAULT_BASE_URL.to_string(),
                api_version: anthropic::DEFAULT_API_VERSION.to_string(),
            })
        );
        assert!(matches!(
            route(&tars_config::default_claude_cli()),
            Route::Cli
        ));
    }
}
