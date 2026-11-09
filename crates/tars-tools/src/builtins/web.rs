//! `web.fetch` and `web.search`: adapters over [`sisurf_core::fetch`] and
//! [`sisurf_core::search`]. They parse the arguments, make the one call and turn sisurf's result
//! into a [`ToolOutput`].
//!
//! They do not fetch, escalate to a browser, distill or parse results themselves, and they do
//! not decide whether a call is allowed; a sandbox that cuts the network refuses them, as it
//! does `bash.run`'s egress. A [`WebError`] means the call ran and failed, so it is
//! an `is_error` result with its own message per variant (a missing browser says to install one);
//! [`ToolError`] is only for bad arguments and cancellation.

use std::sync::OnceLock;
use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use serde::Deserialize;

use sisurf_core::{FetchOpts, Page, SearchConfig, SearchOpts, SearchResult, Tier, WebError};
use tars_types::JsonSchema;

use crate::tool::{Tool, ToolEnv, ToolError, ToolOutput};

// ---------------------------------------------------------------------------
// web.fetch
// ---------------------------------------------------------------------------

/// Fetches a URL and returns its main content as Markdown, with the final URL and the tier that
/// served it.
#[derive(Debug, Default)]
pub struct WebFetchTool;

impl WebFetchTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct WebFetchArgs {
    url: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web.fetch"
    }

    fn description(&self) -> &str {
        "Fetch a web page by URL and return its main content as clean Markdown, \
         plus the final URL and which tier served it (static vs. browser). Use \
         to READ a specific page you already have the URL for (docs, an article, \
         an API reference). To DISCOVER URLs from a query, use web.search first."
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "WebFetchArgs",
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The http(s) URL to fetch."
                        }
                    },
                    "required": ["url"]
                },
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        env: &ToolEnv,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let parsed: WebFetchArgs =
            serde_json::from_value(args).map_err(ToolError::InvalidArguments)?;
        if !env.sandbox.network() {
            return Ok(network_denied("web.fetch"));
        }

        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            r = sisurf_core::fetch(&parsed.url, FetchOpts::default()) => r,
        };

        Ok(match outcome {
            Ok(page) => render_page(page),
            Err(e) => web_error_result("web.fetch", e),
        })
    }
}

/// Returns the refusal of a web call under a sandbox that cuts the network.
fn network_denied(tool: &str) -> ToolOutput {
    ToolOutput::titled_error(
        format!("{tool}: network denied"),
        format!("{tool} reaches the network, and this session's sandbox does not allow it"),
    )
}

/// Returns the page's Markdown under a header naming its final URL and serving tier.
fn render_page(page: Page) -> ToolOutput {
    let tier = tier_label(page.tier);
    let title = format!(
        "Fetched {} ({tier}, {} chars)",
        page.url, page.content_length
    );
    let heading = page.title.as_deref().unwrap_or("(untitled)");
    let content = format!(
        "# {heading}\nURL: {}\nTier: {tier}\n\n{}",
        page.url, page.content
    );
    ToolOutput::titled_success(title, content)
}

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Static => "static",
        Tier::Browser => "browser",
    }
}

// ---------------------------------------------------------------------------
// web.search
// ---------------------------------------------------------------------------

/// Searches the web and returns a numbered list of title, URL and snippet.
///
/// Its [`SearchConfig`] already carries the API key; the backend is built from it on each call,
/// so a missing key or a misconfigured backend is an `is_error` result the agent reads, not a
/// failure to build the tool.
#[derive(Debug, Default)]
pub struct WebSearchTool {
    config: SearchConfig,
}

impl WebSearchTool {
    /// Returns a tool on the DuckDuckGo backend, which needs no API key.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a tool on `config`, whose API key the caller has already filled in.
    pub fn with_config(config: SearchConfig) -> Self {
        Self { config }
    }
}

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web.search"
    }

    fn description(&self) -> &str {
        "Search the web for a query and return a ranked list of results \
         (title, URL, snippet). Use to DISCOVER pages when you don't already \
         have a URL; then follow up with web.fetch to read a specific result."
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "WebSearchArgs",
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query."
                        }
                    },
                    "required": ["query"]
                },
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        env: &ToolEnv,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let parsed: WebSearchArgs =
            serde_json::from_value(args).map_err(ToolError::InvalidArguments)?;
        if !env.sandbox.network() {
            return Ok(network_denied("web.search"));
        }

        // A backend that does not build is reported; there is no fallback to another backend.
        let backend = match self.config.build() {
            Ok(b) => b,
            Err(e) => return Ok(web_error_result("web.search", e)),
        };
        let opts = SearchOpts {
            backend,
            ..Default::default()
        };

        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            r = sisurf_core::search(&parsed.query, opts) => r,
        };

        Ok(match outcome {
            Ok(results) => render_results(&parsed.query, results),
            Err(e) => web_error_result("web.search", e),
        })
    }
}

fn render_results(query: &str, results: Vec<SearchResult>) -> ToolOutput {
    let title = format!("Searched {query:?} ({} results)", results.len());
    let mut body = format!("Results for {query:?}:\n");
    for (i, r) in results.iter().enumerate() {
        body.push_str(&format!("\n{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            body.push_str(&format!("   {}\n", r.snippet));
        }
    }
    ToolOutput::titled_success(title, body)
}

// ---------------------------------------------------------------------------
// Typed error → legible tool result
// ---------------------------------------------------------------------------

/// Returns the `is_error` result for a [`WebError`], with a message per variant that says what
/// to do next.
fn web_error_result(tool: &str, err: WebError) -> ToolOutput {
    let (title, body) = match err {
        WebError::NoBrowser { hint } => (
            format!("{tool}: no browser available"),
            format!(
                "This page needs a headless Chromium-family browser but none could be \
                 launched: {hint}. Install Chrome/Chromium (or configure a browser path), \
                 or fetch a URL that serves its content without client-side JavaScript."
            ),
        ),
        WebError::InvalidUrl { url } => (
            format!("{tool}: invalid URL"),
            format!("`{url}` is not a fetchable http/https URL."),
        ),
        WebError::Http { url, status } => (
            format!("{tool}: HTTP {status}"),
            format!("The server returned HTTP {status} for {url}."),
        ),
        WebError::Fetch(source) => (
            format!("{tool}: network error"),
            format!("Static fetch failed: {source}"),
        ),
        WebError::Browser(source) => (
            format!("{tool}: browser error"),
            format!("Browser-tier fetch failed: {source}"),
        ),
        WebError::EmptyResults { query } => (
            format!("{tool}: no results"),
            format!("The search returned no results for {query:?}. Try different terms."),
        ),
        WebError::SearchParse { backend, detail } => (
            format!("{tool}: could not parse {backend} results"),
            format!(
                "The {backend} search page was fetched but no results could be parsed: {detail}"
            ),
        ),
        WebError::MissingApiKey(backend) => (
            format!("{tool}: {backend} API key missing"),
            format!(
                "Search backend `{backend}` is selected but its API key is empty, so this tool \
                 cannot search until the program that built it fills the key into its \
                 `SearchConfig`, or builds it on `ddg`, which needs no key. No argument to this \
                 call changes that."
            ),
        ),
        WebError::BackendConfig { backend, detail } => (
            format!("{tool}: {backend} misconfigured"),
            format!("Search backend `{backend}` is misconfigured: {detail}"),
        ),
        WebError::UnsupportedBackend(detail) => (
            format!("{tool}: backend unavailable"),
            format!("The selected search backend is not available: {detail}"),
        ),
    };
    ToolOutput::titled_error(title, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    use sisurf_core::FetchError;

    // ── schema / identity ────────────────────────────────────────────

    #[test]
    fn fetch_name_and_schema_pin_the_contract() {
        let t = WebFetchTool::new();
        assert_eq!(t.name(), "web.fetch");
        assert!(t.description().to_lowercase().contains("fetch"));
        let schema = t.input_schema();
        assert!(schema.strict);
        let required: Vec<&str> = schema.schema()["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["url"]);
        assert_eq!(schema.schema()["additionalProperties"], json!(false));
    }

    #[test]
    fn search_name_and_schema_pin_the_contract() {
        let t = WebSearchTool::new();
        assert_eq!(t.name(), "web.search");
        assert!(t.description().to_lowercase().contains("search"));
        let schema = t.input_schema();
        assert!(schema.strict);
        let required: Vec<&str> = schema.schema()["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["query"]);
        assert_eq!(schema.schema()["additionalProperties"], json!(false));
    }

    // ── result mapping (hermetic — no network) ───────────────────────

    #[test]
    fn page_maps_to_success_with_content_and_provenance() {
        let page = Page {
            url: "https://example.com/doc".to_string(),
            title: Some("Example Doc".to_string()),
            content: "# Hello\n\nBody text.".to_string(),
            content_length: 19,
            tier: Tier::Static,
        };
        let r = render_page(page);
        assert!(!r.is_error);
        assert!(r.content.contains("Body text."));
        assert!(r.content.contains("URL: https://example.com/doc"));
        assert!(r.content.contains("Tier: static"), "content: {}", r.content);
        assert!(r.title.as_deref().unwrap_or_default().contains("static"));
    }

    #[test]
    fn results_map_to_numbered_structured_list() {
        let results = vec![
            SearchResult {
                title: "First".to_string(),
                url: "https://a.com".to_string(),
                snippet: "the first hit".to_string(),
            },
            SearchResult {
                title: "Second".to_string(),
                url: "https://b.com".to_string(),
                snippet: String::new(),
            },
        ];
        let r = render_results("rust async", results);
        assert!(!r.is_error);
        assert!(r.content.contains("1. First"));
        assert!(r.content.contains("https://a.com"));
        assert!(r.content.contains("the first hit"));
        assert!(r.content.contains("2. Second"));
        // The second hit has no snippet, so it gets no snippet line.
        assert!(!r.content.contains("Second\n   https://b.com\n   \n"));
        assert!(r.title.as_deref().unwrap_or_default().contains("2 results"));
    }

    #[test]
    fn untitled_page_falls_back_to_placeholder_heading() {
        let page = Page {
            url: "https://example.com/raw".to_string(),
            title: None,
            content: "plain body".to_string(),
            content_length: 10,
            tier: Tier::Browser,
        };
        let r = render_page(page);
        assert!(!r.is_error);
        assert!(r.content.contains("# (untitled)"), "content: {}", r.content);
        assert!(r.title.as_deref().unwrap_or_default().contains("browser"));
        assert!(r.content.contains("Tier: browser"));
    }

    #[test]
    fn empty_result_list_renders_a_zero_result_success_not_an_error() {
        // Zero hits from a backend, unlike `WebError::EmptyResults`, is a success.
        let r = render_results("no such thing", Vec::new());
        assert!(!r.is_error);
        assert!(r.title.as_deref().unwrap_or_default().contains("0 results"));
        assert!(r.content.contains("Results for"));
    }

    // ── typed error mapping — NoBrowser stays legible + branchable ────

    #[test]
    fn no_browser_maps_to_legible_actionable_error_not_a_blob() {
        let err = WebError::NoBrowser {
            hint: "no chromium on PATH".to_string(),
        };
        let r = web_error_result("web.fetch", err);
        assert!(r.is_error);
        assert!(r.content.to_lowercase().contains("browser"));
        assert!(r.content.contains("Install Chrome"));
        assert!(r.content.contains("no chromium on PATH"), "hint preserved");
        assert!(!r.content.contains("NoBrowser"));
        assert!(
            r.title
                .as_deref()
                .unwrap_or_default()
                .contains("no browser available")
        );
    }

    #[test]
    fn error_mapping_branches_per_variant() {
        let no_browser = web_error_result("web.fetch", WebError::NoBrowser { hint: "x".into() });
        let http = web_error_result(
            "web.fetch",
            WebError::Http {
                url: "https://e.com".into(),
                status: 404,
            },
        );
        let invalid = web_error_result("web.fetch", WebError::InvalidUrl { url: "nope".into() });
        assert!(
            no_browser
                .title
                .as_deref()
                .unwrap_or_default()
                .contains("no browser")
        );
        assert!(http.title.as_deref().unwrap_or_default().contains("404"));
        assert!(
            invalid
                .title
                .as_deref()
                .unwrap_or_default()
                .contains("invalid URL")
        );
        for r in [&no_browser, &http, &invalid] {
            assert!(r.is_error);
        }
        assert_ne!(no_browser.title, http.title);
        assert_ne!(http.title, invalid.title);
    }

    #[test]
    fn fetch_source_error_renders_the_real_reason_not_a_sentinel() {
        let err = WebError::Fetch(FetchError::Status(503));
        let r = web_error_result("web.fetch", err);
        assert!(r.is_error);
        assert!(r.content.contains("Static fetch failed"));
        assert!(
            r.content.contains("503"),
            "real status preserved: {}",
            r.content
        );
    }

    #[test]
    fn browser_source_error_renders_the_real_reason_not_a_sentinel() {
        use sisurf_core::BrowserError;
        let err = WebError::Browser(BrowserError::Crashed("cdp websocket dropped".into()));
        let r = web_error_result("web.fetch", err);
        assert!(r.is_error);
        assert!(r.content.contains("Browser-tier fetch failed"));
        assert!(
            r.content.contains("cdp websocket dropped"),
            "real source reason preserved: {}",
            r.content
        );
        assert!(!r.content.contains("Browser("), "no Debug blob");
    }

    #[test]
    fn search_parse_error_names_the_backend_and_carries_the_detail() {
        let r = web_error_result(
            "web.search",
            WebError::SearchParse {
                backend: "ddg".into(),
                detail: "expected <a class=result__a> nodes, found none".into(),
            },
        );
        assert!(r.is_error);
        assert!(r.title.as_deref().unwrap_or_default().contains("ddg"));
        assert!(r.content.contains("expected <a class=result__a> nodes"));
        assert!(!r.content.contains("SearchParse"), "no Debug blob");
    }

    #[test]
    fn unsupported_backend_is_legible() {
        let r = web_error_result(
            "web.search",
            WebError::UnsupportedBackend("compiled without the brave feature".into()),
        );
        assert!(r.is_error);
        assert!(r.content.contains("compiled without the brave feature"));
        assert!(!r.content.contains("UnsupportedBackend"), "no Debug blob");
    }

    #[test]
    fn missing_api_key_and_backend_config_map_legibly() {
        let missing = web_error_result("web.search", WebError::MissingApiKey("google_cse".into()));
        assert!(missing.is_error);
        assert!(missing.content.contains("google_cse"));
        assert!(missing.content.contains("API key is empty"));
        assert!(
            missing.content.contains("SearchConfig") && missing.content.contains("ddg"),
            "says who fills the key in: {}",
            missing.content
        );
        assert!(
            !missing.content.contains("[web_search]"),
            "{}",
            missing.content
        );
        assert!(!missing.content.contains("MissingApiKey"), "no debug blob");

        let misconfig = web_error_result(
            "web.search",
            WebError::BackendConfig {
                backend: "google_cse".into(),
                detail: "missing `cx` programmable-search-engine id".into(),
            },
        );
        assert!(misconfig.is_error);
        assert!(misconfig.content.contains("misconfigured"));
        assert!(misconfig.content.contains("cx"));
    }

    #[tokio::test]
    async fn google_cse_without_key_surfaces_legible_tool_error_before_network() {
        // `build()` fails before any network call, so this test needs no network.
        use sisurf_core::{BackendKind, GoogleCseConfig};
        let config = SearchConfig {
            backend: BackendKind::GoogleCse,
            google_cse: Some(GoogleCseConfig {
                cx: "some-cx".into(),
                api_key: String::new(),
            }),
            brave: None,
        };
        let tool: Arc<dyn Tool> = Arc::new(WebSearchTool::with_config(config));
        let r = tool
            .execute(
                json!({"query": "anything"}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("google_cse"));
        assert!(r.content.to_lowercase().contains("api key"));
    }

    #[test]
    fn default_search_tool_builds_ddg_backend() {
        let tool = WebSearchTool::new();
        assert!(matches!(
            tool.config.build(),
            Ok(sisurf_core::SearchBackend::DdgScrape)
        ));
    }

    #[test]
    fn empty_results_is_legible() {
        let r = web_error_result(
            "web.search",
            WebError::EmptyResults {
                query: "asdfqwerty".into(),
            },
        );
        assert!(r.is_error);
        assert!(r.content.contains("no results"));
        assert!(r.content.contains("asdfqwerty"));
    }

    // ── bad args + cancellation are hard ToolErrors, not is_error ─────

    #[tokio::test]
    async fn fetch_invalid_args_is_typed_invalid_arguments() {
        let tool: Arc<dyn Tool> = Arc::new(WebFetchTool::new());
        let err = tool
            .execute(
                json!({"not_url": "x"}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .expect_err("missing url must reject");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn search_invalid_args_is_typed_invalid_arguments() {
        let tool: Arc<dyn Tool> = Arc::new(WebSearchTool::new());
        let err = tool
            .execute(
                json!({"q": "x"}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .expect_err("missing query must reject");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn fetch_cancelled_before_work_is_typed_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolEnv {
            ..Default::default()
        };
        let tool: Arc<dyn Tool> = Arc::new(WebFetchTool::new());
        let err = tool
            .execute(json!({"url": "https://example.com"}), &ctx, cancel)
            .await
            .expect_err("pre-cancelled should fast-fail");
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn search_cancelled_before_work_is_typed_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolEnv {
            ..Default::default()
        };
        let tool: Arc<dyn Tool> = Arc::new(WebSearchTool::new());
        let err = tool
            .execute(json!({"query": "anything"}), &ctx, cancel)
            .await
            .expect_err("pre-cancelled should fast-fail");
        assert!(matches!(err, ToolError::Cancelled));
    }

    // ── registration + spec projection ────────────────────────────────

    #[tokio::test]
    async fn both_web_tools_register_and_expose_specs() {
        use crate::ToolRegistry;

        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(WebFetchTool::new())).unwrap();
        reg.register(Arc::new(WebSearchTool::new())).unwrap();

        let names: Vec<String> = reg.to_tool_specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"web.fetch".to_string()), "specs: {names:?}");
        assert!(
            names.contains(&"web.search".to_string()),
            "specs: {names:?}"
        );
    }

    #[tokio::test]
    async fn a_sandbox_without_network_keeps_web_fetch_off_the_wire() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let env = ToolEnv {
            sandbox: crate::SandboxPolicy::read_only(false),
            ..Default::default()
        };
        let r = WebFetchTool::new()
            .execute(
                serde_json::json!({ "url": format!("http://127.0.0.1:{port}/") }),
                &env,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let connected = AtomicBool::new(listener.accept().is_ok());
        assert!(
            !connected.load(Ordering::SeqCst),
            "web.fetch connected: {}",
            r.content
        );
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("does not allow it"), "{}", r.content);
    }

    #[tokio::test]
    async fn a_sandbox_without_network_refuses_web_search() {
        let env = ToolEnv {
            sandbox: crate::SandboxPolicy::read_only(false),
            ..Default::default()
        };
        let r = WebSearchTool::new()
            .execute(
                serde_json::json!({ "query": "rust" }),
                &env,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert_eq!(r.title.as_deref(), Some("web.search: network denied"));
    }
}
