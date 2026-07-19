//! The reusable **map-reduce code reviewer**: build an [`LlmService`] over a real
//! OpenAI-compatible provider (DeepSeek) — or a mock with the same call path — review a file's
//! content through it, and parse the model's JSON into stable [`Finding`]s.
//!
//! This is the shared core behind both `examples/review` (the world-native memo demo over the
//! planted-bug corpus) and `src/bin/review-cli.rs` (the file-taking CLI the VSCode frontend
//! spawns). The LLM call goes through [`tars_pipeline::LlmService`] — never raw HTTP.

use std::sync::Arc;

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use tars_pipeline::LlmService;
use tars_provider::backends::mock::{CannedResponse, MockProvider};
use tars_provider::{Auth, HttpProviderBase, LlmProvider, OpenAiProviderBuilder, basic};
use tars_types::{ChatEvent, ChatRequest, RequestContext, TraceId};

/// Default model for the DeepSeek backend.
pub const MODEL: &str = "deepseek-chat";
/// DeepSeek base URL — contains "deepseek", so the OpenAI adapter auto-selects `DeepSeekDialect`.
pub const BASE_URL: &str = "https://api.deepseek.com";

/// **The stable JSON shape the VSCode frontend consumes** — one review comment per finding.
/// Keep these field names stable; the extension parses this exact shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// The file the finding is about — echoed back exactly as the path was passed to the
    /// reviewer (the CLI passes absolute paths, so the extension can `Uri.file(file)` directly).
    pub file: String,
    /// 1-based line, or `null` when the model didn't localize the bug.
    pub line: Option<u32>,
    /// Human-readable description of the bug.
    pub message: String,
    /// Lifecycle status. Starts `"open"`; a fix loop would flip it to `"fixed"`.
    pub status: String,
    /// Model-assigned severity (`"high" | "med" | "low"`), best-effort.
    pub severity: String,
}

/// What the model returns per file — the wire shape of one entry in `{"findings":[...]}`.
#[derive(Debug, Clone, Deserialize)]
struct RawFinding {
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    severity: String,
    message: String,
}

#[derive(Deserialize)]
struct FindingsEnvelope {
    findings: Vec<RawFinding>,
}

/// Which provider backed the run — reported so the output is never ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The real DeepSeek provider via `OpenAiProviderBuilder`.
    RealDeepSeek,
    /// A `MockProvider` replaying a canned stream (same `LlmService` call path).
    Mock,
}

impl Backend {
    /// Whether a real provider is configured (a non-empty `DEEPSEEK_API_KEY`).
    pub fn detect() -> Backend {
        match std::env::var("DEEPSEEK_API_KEY") {
            Ok(k) if !k.trim().is_empty() => Backend::RealDeepSeek,
            _ => Backend::Mock,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Backend::RealDeepSeek => "REAL DeepSeek (OpenAiProviderBuilder → LlmService)",
            Backend::Mock => "MOCK (no DEEPSEEK_API_KEY; canned stream through the real LlmService path)",
        }
    }
}

/// Build the real DeepSeek-backed [`LlmService`], or `None` if no API key is configured. **This
/// is the construction under test** — the repo's OpenAI-compatible provider, pointed at DeepSeek,
/// wrapped in a pipeline service. No raw HTTP.
pub fn build_real_service() -> Option<LlmService> {
    let key = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(k) if !k.trim().is_empty() => k,
        _ => return None,
    };
    let http = HttpProviderBase::default_arc().expect("http base");
    let provider: Arc<dyn LlmProvider> = OpenAiProviderBuilder::new("deepseek", Auth::inline(key))
        .base_url(BASE_URL)
        .build(http, basic());
    Some(LlmService::builder(provider, MODEL).build())
}

/// Build a mock-backed [`LlmService`] that replays `canned_json` (a `{"findings":[...]}` body) as
/// the model's response. Same `LlmService::call` → `LlmEventStream` path as the real provider.
pub fn build_mock_service(id: impl Into<String>, canned_json: String) -> LlmService {
    let provider = MockProvider::new(id.into(), CannedResponse::Text(canned_json));
    LlmService::builder(provider, MODEL).build()
}

/// Review one file's content through the pipeline. Drains the `LlmEventStream`, accumulating text
/// deltas, then tolerantly parses `{"findings":[...]}`. On any failure it surfaces the truth (the
/// real error / the raw model text) to stderr and returns no findings — never a sentinel.
pub async fn review_one(service: &LlmService, file_label: &str, content: &str) -> Vec<Finding> {
    let system = "You are a strict Rust code reviewer. Return ONLY a JSON object of the form \
        {\"findings\":[{\"line\":<int|null>,\"severity\":\"high|med|low\",\"message\":\"...\"}]} \
        listing every real bug in the file. Line numbers are 1-based. No prose, no code fences.";
    let user = format!("Review this Rust file `{file_label}` and report every bug as JSON:\n\n{content}");
    let req = ChatRequest::user(user).with_system(system);
    let ctx = RequestContext::personal(TraceId::new(format!("review-{file_label}")));

    let mut stream = match service.call(req, ctx).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  [review] {file_label}: LlmService.call failed: {e}");
            return Vec::new();
        }
    };

    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ChatEvent::Delta { text: t }) => text.push_str(&t),
            Ok(_) => {}
            Err(e) => eprintln!("  [review] {file_label}: stream error: {e}"),
        }
    }

    parse_findings(file_label, &text)
}

/// Tolerant parse: try the whole body as the envelope, else slice from the first `{` to the last
/// `}` (models sometimes wrap JSON in prose despite instructions). On total failure, surface the
/// raw text (truncated) — the real thing the model said — not an opaque token.
pub fn parse_findings(file_label: &str, raw: &str) -> Vec<Finding> {
    let envelope = serde_json::from_str::<FindingsEnvelope>(raw).ok().or_else(|| {
        let start = raw.find('{')?;
        let end = raw.rfind('}')?;
        if end > start {
            serde_json::from_str::<FindingsEnvelope>(&raw[start..=end]).ok()
        } else {
            None
        }
    });

    match envelope {
        Some(env) => env
            .findings
            .into_iter()
            .map(|rf| Finding {
                file: file_label.to_string(),
                line: rf.line,
                message: rf.message,
                status: "open".to_string(),
                severity: if rf.severity.is_empty() {
                    "unknown".to_string()
                } else {
                    rf.severity
                },
            })
            .collect(),
        None => {
            eprintln!(
                "  [review] {file_label}: could not parse findings JSON; raw model text: {}",
                truncate(raw, 200)
            );
            Vec::new()
        }
    }
}

/// Dedup findings by `(file, line, message)`, preserving first-seen order.
pub fn dedup(findings: Vec<Finding>) -> Vec<Finding> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for f in findings {
        let key = (f.file.clone(), f.line, f.message.clone());
        if seen.insert(key) {
            out.push(f);
        }
    }
    out
}

/// Truncate a string to `n` chars with an ellipsis — used to surface raw model text on a parse
/// failure without dumping an unbounded blob.
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
