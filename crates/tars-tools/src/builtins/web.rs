//! `web.fetch` + `web.search` — the web capability, backed by `sisurf-core`.
//!
//! Two thin adapters over sisurf's typed public API:
//!
//!   * [`WebFetchTool`] (`web.fetch`) — URL → clean Markdown + provenance
//!     (final URL, which tier served it), via [`sisurf_core::fetch`]. sisurf
//!     hides the reqwest→Chromium escalation; the tool never picks a tier.
//!   * [`WebSearchTool`] (`web.search`) — query → a structured
//!     title/url/snippet list, via [`sisurf_core::search`].
//!
//! ## No web logic here
//!
//! All fetching, escalation, distillation, and result parsing lives in
//! `sisurf-core`. These tools only (a) validate/parse the JSON args, (b) call
//! the one sisurf primitive, and (c) adapt sisurf's typed result / typed
//! [`WebError`] into the tars [`ToolResult`] contract.
//!
//! ## Network op → gated
//!
//! Both are `web.*` (network, long-running). The dispatch gate
//! ([`crate::ToolRegistry::dispatch`]) enforces permission/approval by tool
//! name before `execute` is ever reached, so a policy that marks `web.fetch` /
//! `web.search` as `Ask`/`Deny` routes them through the human-approval sink
//! exactly like `bash.run` — no per-tool plumbing needed here.
//!
//! ## Typed errors, not stringified blobs
//!
//! sisurf returns a typed [`WebError`]. We **branch on the variant** (see
//! [`web_error_result`]) rather than collapsing it to one string, so
//! [`WebError::NoBrowser`] surfaces as its own legible, actionable message
//! ("needs a headless browser — install Chrome, or it fell back to static")
//! that the calling agent can read and adapt to. A `WebError` means the op ran
//! but failed, so it becomes an `is_error` [`ToolResult`] (the LLM should
//! adapt: fix the URL, install a browser, retry a query) — not a hard
//! [`ToolError`], which is reserved for "couldn't even attempt" (bad args,
//! cancelled).

use std::sync::OnceLock;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use sisurf_core::{FetchOpts, Page, SearchConfig, SearchOpts, SearchResult, Tier, WebError};
use tars_types::JsonSchema;

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

// ---------------------------------------------------------------------------
// web.fetch
// ---------------------------------------------------------------------------

/// `web.fetch` — fetch a URL and return its clean Markdown + metadata.
#[derive(Debug, Default)]
pub struct WebFetchTool;

impl WebFetchTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct WebFetchArgs {
    /// The http(s) URL to fetch.
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
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The http(s) URL to fetch."
                        }
                    },
                    "required": ["url"]
                }),
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: WebFetchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        // Race the network op against cooperative cancel so an upstream Drop /
        // SIGINT aborts the await rather than waiting out the fetch.
        let outcome = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = sisurf_core::fetch(&parsed.url, FetchOpts::default()) => r,
        };

        Ok(match outcome {
            Ok(page) => render_page(page),
            Err(e) => web_error_result("web.fetch", e),
        })
    }
}

/// Format a fetched [`Page`] into the tool's success result: clean Markdown
/// body, with the final URL + serving tier as a small provenance header so the
/// model knows what it's reading (and whether a browser was needed).
fn render_page(page: Page) -> ToolResult {
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
    ToolResult::titled_success(title, content)
}

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Static => "static",
        Tier::Browser => "browser",
    }
}

/// Human hint naming the env var the consumer resolves a backend's API key
/// from. Kept as a legible message aid only — the authoritative env-var name
/// lives in `tars-config` (the crate that actually resolves + injects the key);
/// tars-tools stays a leaf and never reads the environment itself.
fn api_key_env_hint(backend: &str) -> &'static str {
    match backend {
        "google_cse" => "e.g. GOOGLE_CSE_KEY",
        "brave" => "e.g. BRAVE_API_KEY",
        _ => "the backend's API key env var",
    }
}

// ---------------------------------------------------------------------------
// web.search
// ---------------------------------------------------------------------------

/// `web.search` — search the web and return a structured result list.
///
/// Holds a sisurf [`SearchConfig`] (which backend + its parameters). The
/// **key is already injected by the consumer** (tars-config resolves an env
/// var and writes it into the config) — sisurf never reads the environment.
/// The runnable backend is produced by [`SearchConfig::build`] at call time so
/// a missing key / misconfigured backend surfaces as a legible tool error the
/// agent can read, rather than failing tool construction opaquely.
#[derive(Debug, Default)]
pub struct WebSearchTool {
    /// The (key-injected) search backend config. `Default` = DuckDuckGo scrape,
    /// which needs no key and works out of the box.
    config: SearchConfig,
}

impl WebSearchTool {
    /// Default DuckDuckGo backend (no API key required).
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a specific, already-key-injected [`SearchConfig`] (Google CSE /
    /// Brave / DDG). The consumer resolves any API key from the environment and
    /// injects it into `config` before constructing the tool.
    pub fn with_config(config: SearchConfig) -> Self {
        Self { config }
    }
}

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    /// The search query.
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
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query."
                        }
                    },
                    "required": ["query"]
                }),
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: WebSearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        // Resolve the configured backend. A missing key / misconfigured backend
        // is a typed `WebError` (MissingApiKey / BackendConfig) — surface it as
        // a legible tool error, don't silently fall back to another backend.
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
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = sisurf_core::search(&parsed.query, opts) => r,
        };

        Ok(match outcome {
            Ok(results) => render_results(&parsed.query, results),
            Err(e) => web_error_result("web.search", e),
        })
    }
}

/// Format the search hits into a compact, numbered title/url/snippet list.
fn render_results(query: &str, results: Vec<SearchResult>) -> ToolResult {
    let title = format!("Searched {query:?} ({} results)", results.len());
    let mut body = format!("Results for {query:?}:\n");
    for (i, r) in results.iter().enumerate() {
        body.push_str(&format!("\n{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            body.push_str(&format!("   {}\n", r.snippet));
        }
    }
    ToolResult::titled_success(title, body)
}

// ---------------------------------------------------------------------------
// Typed error → legible tool result
// ---------------------------------------------------------------------------

/// Adapt a sisurf [`WebError`] into an `is_error` [`ToolResult`], **branching
/// on the variant** so each failure mode gets its own legible, actionable
/// message — never a `format!("{:?}")` blob or a blanket `.to_string()`.
///
/// [`WebError::NoBrowser`] in particular stays a first-class case: the agent is
/// told a headless browser was needed and how to recover, so it can adapt (ask
/// the user to install Chrome, or accept the static fallback) rather than
/// re-guessing at an opaque string. Where a variant carries a typed `#[source]`
/// (`Fetch`, `Browser`), we render *that source's* own `Display` — the real
/// reason — instead of inventing a sentinel.
fn web_error_result(tool: &str, err: WebError) -> ToolResult {
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
            format!("The {backend} search page was fetched but no results could be parsed: {detail}"),
        ),
        WebError::MissingApiKey(backend) => (
            format!("{tool}: {backend} API key missing"),
            format!(
                "Search backend `{backend}` is selected but its API key is empty. Set the \
                 backend's API key environment variable ({}) in your tars secrets config, or \
                 switch `[web_search] backend` to `ddg` (no key required).",
                api_key_env_hint(&backend),
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
    ToolResult::titled_error(title, body)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let required: Vec<&str> = schema.schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["url"]);
        assert_eq!(schema.schema["additionalProperties"], json!(false));
    }

    #[test]
    fn search_name_and_schema_pin_the_contract() {
        let t = WebSearchTool::new();
        assert_eq!(t.name(), "web.search");
        assert!(t.description().to_lowercase().contains("search"));
        let schema = t.input_schema();
        assert!(schema.strict);
        let required: Vec<&str> = schema.schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["query"]);
        assert_eq!(schema.schema["additionalProperties"], json!(false));
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
        assert!(r.title.contains("static"));
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
        assert!(r.title.contains("2 results"));
    }

    // ── typed error mapping — NoBrowser stays legible + branchable ────

    #[test]
    fn no_browser_maps_to_legible_actionable_error_not_a_blob() {
        let err = WebError::NoBrowser {
            hint: "no chromium on PATH".to_string(),
        };
        let r = web_error_result("web.fetch", err);
        assert!(r.is_error);
        // Legible + actionable: names the cause and the remedy.
        assert!(r.content.to_lowercase().contains("browser"));
        assert!(r.content.contains("Install Chrome"));
        assert!(r.content.contains("no chromium on PATH"), "hint preserved");
        // NOT a Debug blob of the enum.
        assert!(!r.content.contains("NoBrowser"));
        assert!(r.title.contains("no browser available"));
    }

    #[test]
    fn error_mapping_branches_per_variant() {
        // Distinct variants ⇒ distinct legible messages (we branch on the
        // typed variant, we don't stringify one blob).
        let no_browser = web_error_result(
            "web.fetch",
            WebError::NoBrowser { hint: "x".into() },
        );
        let http = web_error_result(
            "web.fetch",
            WebError::Http {
                url: "https://e.com".into(),
                status: 404,
            },
        );
        let invalid = web_error_result(
            "web.fetch",
            WebError::InvalidUrl { url: "nope".into() },
        );
        assert!(no_browser.title.contains("no browser"));
        assert!(http.title.contains("404"));
        assert!(invalid.title.contains("invalid URL"));
        // All three are error results, all with different titles.
        for r in [&no_browser, &http, &invalid] {
            assert!(r.is_error);
        }
        assert_ne!(no_browser.title, http.title);
        assert_ne!(http.title, invalid.title);
    }

    #[test]
    fn fetch_source_error_renders_the_real_reason_not_a_sentinel() {
        // A typed transport source is carried through as its own Display —
        // the real reason, not `parse_failed`/`unknown`.
        let err = WebError::Fetch(FetchError::Status(503));
        let r = web_error_result("web.fetch", err);
        assert!(r.is_error);
        assert!(r.content.contains("Static fetch failed"));
        assert!(r.content.contains("503"), "real status preserved: {}", r.content);
    }

    #[test]
    fn missing_api_key_and_backend_config_map_legibly() {
        let missing = web_error_result("web.search", WebError::MissingApiKey("google_cse".into()));
        assert!(missing.is_error);
        assert!(missing.content.contains("google_cse"));
        assert!(missing.content.contains("API key is empty"));
        assert!(missing.content.contains("GOOGLE_CSE_KEY"), "names the env var: {}", missing.content);
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
        // A google_cse backend with no injected key must fail at `.build()`
        // (MissingApiKey) and surface as a legible is_error tool result — never
        // a silent fall-through to DDG, and never a network call.
        use sisurf_core::{BackendKind, GoogleCseConfig};
        let config = SearchConfig {
            backend: BackendKind::GoogleCse,
            google_cse: Some(GoogleCseConfig {
                cx: "some-cx".into(),
                api_key: String::new(), // consumer resolved no key
            }),
            brave: None,
        };
        let tool: Arc<dyn Tool> = Arc::new(WebSearchTool::with_config(config));
        let r = tool
            .execute(json!({"query": "anything"}), ToolContext::default())
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("google_cse"));
        assert!(r.content.to_lowercase().contains("api key"));
    }

    #[test]
    fn default_search_tool_builds_ddg_backend() {
        // Out-of-the-box (no config) must resolve to the keyless DDG backend.
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
            .execute(json!({"not_url": "x"}), ToolContext::default())
            .await
            .expect_err("missing url must reject");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn search_invalid_args_is_typed_invalid_arguments() {
        let tool: Arc<dyn Tool> = Arc::new(WebSearchTool::new());
        let err = tool
            .execute(json!({"q": "x"}), ToolContext::default())
            .await
            .expect_err("missing query must reject");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn fetch_cancelled_before_work_is_typed_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolContext {
            cancel,
            ..Default::default()
        };
        let tool: Arc<dyn Tool> = Arc::new(WebFetchTool::new());
        let err = tool
            .execute(json!({"url": "https://example.com"}), ctx)
            .await
            .expect_err("pre-cancelled should fast-fail");
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn search_cancelled_before_work_is_typed_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolContext {
            cancel,
            ..Default::default()
        };
        let tool: Arc<dyn Tool> = Arc::new(WebSearchTool::new());
        let err = tool
            .execute(json!({"query": "anything"}), ctx)
            .await
            .expect_err("pre-cancelled should fast-fail");
        assert!(matches!(err, ToolError::Cancelled));
    }
}
