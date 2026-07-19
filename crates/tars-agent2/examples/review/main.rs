//! `cargo run --example review` — a **map-reduce code review on agent-2** that emits structured,
//! machine-readable [`Finding`]s over a 5-file sample repo, ported from the arc
//! `observe-operate-review` v11 world-model journey.
//!
//! ## What this is (and the honest design split)
//!
//! - **The review pass is map-reduce** (the primary deliverable): each file is reviewed
//!   *independently* by the LLM (the **map**), then the per-file findings are collected + deduped
//!   (the **reduce**). Structured findings out — the frontend (Phase 3) reads this JSON.
//! - **Findings are a derived, memoized view over versioned [`File`] components.** Each file is a
//!   `tars_agent2::File` (content-hash `version`). The review cache is keyed on `(file id,
//!   version)`; a file not changed since last review is a **memo hit** — reused, never
//!   re-reviewed (v11 finding G — this is what kept the noisy reviewer from oscillating).
//! - **The LLM call goes through `tars_pipeline::LlmService`** built over the repo's real
//!   OpenAI-compatible provider (`OpenAiProviderBuilder`) pointed at DeepSeek — NOT raw reqwest,
//!   NOT (when a key is present) a mock. If `DEEPSEEK_API_KEY` is unset we fall back to a
//!   `MockProvider`, but the **code path is identical**: `service.call(req, ctx)` → drain
//!   `LlmEventStream`. The run prints which provider it used.
//!
//! ## Why this demo does NOT drive `Runtime`/`Check` to a fixed point
//!
//! An LLM re-review is a **judgment, not a deterministic oracle** — it re-flags inputs it just
//! passed, so "reconcile findings → 0" has **no reachable fixed point** and only oscillates
//! (measured, doc 14 §3.7). agent-2's [`tars_agent2::Check`] trait is deliberately **sync and
//! deterministic** — you *cannot* express a noisy async LLM judgment as a `Check` without
//! blocking on it, which is precisely the guardrail: fixpoint reconciliation is for cheap
//! deterministic diffs (build/test/lint — see the crate's `tests/reconcile.rs`, the
//! "make cargo test green" CUJ), not for LLM review. The review-comments use case only needs the
//! review to *produce* findings; we do not fake convergence. A fixer loop over these findings
//! would be fuel-bounded best-effort, exactly as the retrospective reported.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use tars_agent2::{File, World};
use tars_pipeline::LlmService;
use tars_provider::{Auth, HttpProviderBase, LlmProvider, OpenAiProviderBuilder, basic};
use tars_provider::backends::mock::{CannedResponse, MockProvider};
use tars_types::{ChatEvent, ChatRequest, RequestContext, TraceId};

const MODEL: &str = "deepseek-chat";
const BASE_URL: &str = "https://api.deepseek.com"; // contains "deepseek" → DeepSeekDialect auto-selected

/// The 5-file planted-bug corpus (copied from arc `observe-operate-review/sample`), embedded at
/// compile time and written to a scratch dir at runtime so the disk-backed `File` components
/// operate on a throwaway copy (a fixer's writes never touch the committed corpus).
const SAMPLE: &[(&str, &str)] = &[
    ("buggy.rs", include_str!("sample/buggy.rs")),
    ("cache.rs", include_str!("sample/cache.rs")),
    ("math.rs", include_str!("sample/math.rs")),
    ("parser.rs", include_str!("sample/parser.rs")),
    ("util.rs", include_str!("sample/util.rs")),
];

// ===========================================================================
// The frontend contract: a structured, machine-readable finding.
// ===========================================================================

/// **The stable JSON shape the VSCode frontend consumes** (one review comment per finding).
/// Keep these field names stable.
#[derive(Debug, Clone, Serialize)]
struct Finding {
    /// Repo-relative file the finding is about.
    file: String,
    /// 1-based line, or `null` when the model didn't localize it.
    line: Option<u32>,
    /// Human-readable description of the bug.
    message: String,
    /// Lifecycle status. Starts `"open"`; a fix loop would flip it to `"fixed"`.
    status: String,
    /// Model-assigned severity (`"high" | "med" | "low"`), best-effort.
    severity: String,
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

// ===========================================================================
// Building the LlmService — real DeepSeek (via OpenAiProviderBuilder) or a mock.
// ===========================================================================

/// Which provider actually backed the run — reported so the output is never ambiguous.
enum Backend {
    RealDeepSeek,
    Mock,
}

/// One `LlmService` per file. Real mode: every entry wraps the same DeepSeek provider (cheap Arc
/// clone) so the per-file map runs concurrently. Mock mode: each entry replays that file's canned
/// findings (so the fallback output is still per-file meaningful) — but the call path is the same
/// real `LlmService::call` → `LlmEventStream`.
fn build_services() -> (HashMap<String, LlmService>, Backend) {
    let mut services = HashMap::new();

    match std::env::var("DEEPSEEK_API_KEY") {
        Ok(key) if !key.trim().is_empty() => {
            // THE construction under test — the repo's OpenAI-compatible provider, pointed at
            // DeepSeek, wrapped in a pipeline LlmService. No raw HTTP.
            let http = HttpProviderBase::default_arc().expect("http base");
            let provider: Arc<dyn LlmProvider> =
                OpenAiProviderBuilder::new("deepseek", Auth::inline(key))
                    .base_url(BASE_URL)
                    .build(http, basic());
            let service = LlmService::builder(provider, MODEL).build();
            for (name, _) in SAMPLE {
                services.insert(name.to_string(), service.clone());
            }
            (services, Backend::RealDeepSeek)
        }
        _ => {
            // Hermetic fallback: a mock provider per file, replaying that file's canned findings
            // as the model's JSON response. Same LlmService code path.
            for (name, _) in SAMPLE {
                let canned = CannedResponse::Text(mock_findings_json(name));
                let provider = MockProvider::new(format!("mock-{name}"), canned);
                let service = LlmService::builder(provider, MODEL).build();
                services.insert(name.to_string(), service);
            }
            (services, Backend::Mock)
        }
    }
}

/// Canned per-file findings for the offline/mock path (hand-authored to match the planted bugs),
/// so a keyless run still emits a realistic structured payload through the real pipeline.
fn mock_findings_json(file: &str) -> String {
    let findings = match file {
        "buggy.rs" => r#"[{"line":6,"severity":"high","message":"off-by-one: xs[xs.len()] indexes one past the last element and panics"},{"line":13,"severity":"med","message":"parse::<u16>().unwrap() panics on non-numeric input instead of returning an error"},{"line":21,"severity":"high","message":"sum_first indexes xs[i] for 0..n without bounds-checking n against xs.len()"}]"#,
        "cache.rs" => r#"[{"line":13,"severity":"med","message":"put never enforces cap — unbounded growth"},{"line":18,"severity":"high","message":"len()-1 underflows (usize) and panics when the cache is empty"}]"#,
        "math.rs" => r#"[{"line":6,"severity":"high","message":"percent divides by whole without a zero check — divide-by-zero panic"},{"line":13,"severity":"med","message":"factorial loops 1..n (excludes n) — off-by-one, returns (n-1)!"}]"#,
        "parser.rs" => r#"[{"line":8,"severity":"high","message":"parse_kv unwrap() panics when there is no '=' in the input"},{"line":16,"severity":"high","message":"first_token indexes tokens[0] without checking for empty input"}]"#,
        "util.rs" => r#"[{"line":7,"severity":"high","message":"average divides by xs.len() with no empty check — divide-by-zero panic"}]"#,
        _ => "[]",
    };
    format!(r#"{{"findings":{findings}}}"#)
}

// ===========================================================================
// The review — map (per file, via LlmService) + tolerant JSON parse.
// ===========================================================================

/// Review one file's content through the pipeline. Drains the `LlmEventStream`, accumulating text
/// deltas, then tolerantly parses `{"findings":[...]}`. On any failure it surfaces the truth
/// (the real error / the raw model text) to stderr and returns no findings — never a sentinel.
async fn review_one(service: &LlmService, file: &str, content: &str) -> Vec<Finding> {
    let system = "You are a strict Rust code reviewer. Return ONLY a JSON object of the form \
        {\"findings\":[{\"line\":<int|null>,\"severity\":\"high|med|low\",\"message\":\"...\"}]} \
        listing every real bug in the file. Line numbers are 1-based. No prose, no code fences.";
    let user = format!("Review this Rust file `{file}` and report every bug as JSON:\n\n{content}");
    let req = ChatRequest::user(user).with_system(system);
    let ctx = RequestContext::personal(TraceId::new(format!("review-{file}")));

    let mut stream = match service.call(req, ctx).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  [review] {file}: LlmService.call failed: {e}");
            return Vec::new();
        }
    };

    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ChatEvent::Delta { text: t }) => text.push_str(&t),
            Ok(_) => {}
            Err(e) => eprintln!("  [review] {file}: stream error: {e}"),
        }
    }

    parse_findings(file, &text)
}

/// Tolerant parse: try the whole body as the envelope, else slice from the first `{` to the last
/// `}` (models sometimes wrap JSON in prose despite instructions). On total failure, surface the
/// raw text (truncated) — the real thing the model said — not an opaque token.
fn parse_findings(file: &str, raw: &str) -> Vec<Finding> {
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
                file: file.to_string(),
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
                "  [review] {file}: could not parse findings JSON; raw model text: {}",
                truncate(raw, 200)
            );
            Vec::new()
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

// ===========================================================================
// The memoized, world-native review pass.
// ===========================================================================

/// The review cache: `(file id, content version) → findings`. The derived "open findings" view is
/// this cache read back at each file's CURRENT version — no separate status store (v11 finding 2).
type ReviewCache = HashMap<(String, u64), Vec<Finding>>;

/// Reactively review every file NOT already cached at its current version (the map), memoizing on
/// `(id, version)`. Returns `(llm_calls, memo_hits)`.
async fn ensure_reviewed(
    world: &World,
    services: &HashMap<String, LlmService>,
    paths: &HashMap<String, std::path::PathBuf>,
    cache: &mut ReviewCache,
) -> (u32, u32) {
    // Collect the (id, version) work-list up front (immutable borrow of the world).
    let worklist: Vec<(String, u64)> = world
        .components()
        .map(|c| (c.id(), c.version()))
        .collect();

    let mut to_review: Vec<(String, u64, String)> = Vec::new();
    let mut memo_hits = 0u32;
    for (id, version) in worklist {
        if cache.contains_key(&(id.clone(), version)) {
            memo_hits += 1; // memo hit — seen at this exact content, not re-reviewed
            continue;
        }
        // The file's current on-disk content IS the source of truth (File is disk-backed; its
        // version = hash of this content). A fixer's write would bump the version → a miss here.
        let content = std::fs::read_to_string(&paths[&id]).unwrap_or_default();
        to_review.push((id, version, content));
    }

    // MAP: review the misses independently and concurrently, each through its own LlmService.
    let futs = to_review.iter().map(|(id, _v, content)| {
        let service = &services[id];
        async move { review_one(service, id, content).await }
    });
    let results = futures::future::join_all(futs).await;

    // REDUCE (into the cache, keyed per file version).
    let llm_calls = to_review.len() as u32;
    for ((id, version, _content), findings) in to_review.into_iter().zip(results) {
        cache.insert((id, version), findings);
    }
    (llm_calls, memo_hits)
}

/// The derived view: all cached findings for each file's current version, deduped by
/// `(file, line, message)`. This is the structured payload the frontend renders.
fn derived_findings(world: &World, cache: &ReviewCache) -> Vec<Finding> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for c in world.components() {
        if let Some(findings) = cache.get(&(c.id(), c.version())) {
            for f in findings {
                let key = (f.file.clone(), f.line, f.message.clone());
                if seen.insert(key) {
                    out.push(f.clone());
                }
            }
        }
    }
    out
}

// ===========================================================================
// main
// ===========================================================================

#[tokio::main]
async fn main() {
    // Write the embedded corpus to a scratch dir; open each as a versioned File component.
    let dir = std::env::temp_dir().join(format!("agent2-review-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let mut world = World::new();
    let mut paths: HashMap<String, std::path::PathBuf> = HashMap::new();
    for (name, content) in SAMPLE {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("seed sample file");
        world = world.with(File::open(*name, &path).expect("open File component"));
        paths.insert((*name).to_string(), path);
    }

    let (services, backend) = build_services();
    let backend_label = match backend {
        Backend::RealDeepSeek => "REAL DeepSeek (OpenAiProviderBuilder → LlmService)",
        Backend::Mock => "MOCK (no DEEPSEEK_API_KEY; canned stream through the real LlmService path)",
    };
    eprintln!("agent-2 review demo — provider: {backend_label}");
    eprintln!("model: {MODEL}   files: {}", SAMPLE.len());

    let mut cache: ReviewCache = HashMap::new();

    // Pass 1 — every file is unseen → reviewed (the map).
    let (calls1, hits1) = ensure_reviewed(&world, &services, &paths, &mut cache).await;
    eprintln!("  [pass 1] llm reviews: {calls1}   memo hits: {hits1}");

    // Pass 2 — nothing changed → every file is a memo hit (0 llm calls). Proves the seen@version
    // memo (a re-review of unchanged content is never re-sampled).
    let (calls2, hits2) = ensure_reviewed(&world, &services, &paths, &mut cache).await;
    eprintln!("  [pass 2] llm reviews: {calls2}   memo hits: {hits2}  (unchanged files reused)");

    let findings = derived_findings(&world, &cache);
    eprintln!(
        "  findings: {} across {} files\n",
        findings.len(),
        SAMPLE.len()
    );

    // THE frontend contract: structured findings JSON on stdout (logs went to stderr).
    let json = serde_json::to_string_pretty(&findings).expect("serialize findings");
    println!("{json}");

    let _ = std::fs::remove_dir_all(&dir);
}
