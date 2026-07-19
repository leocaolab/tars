//! `cargo run --example review` — the **world-native, memoized** map-reduce review over the arc
//! `observe-operate-review` v11 planted-bug corpus. It layers the demo-specific pieces (the
//! versioned `File` components + the `seen@version` memo + the two-pass proof) on top of the
//! reusable reviewer in [`tars_agent2::review`].
//!
//! The reviewer core (building an `LlmService` over the real DeepSeek provider or a mock with the
//! same call path, reviewing a file, parsing findings) lives in `tars_agent2::review` and is
//! shared with `src/bin/review-cli.rs` — this example adds the world model on top:
//!
//! - each file is a `tars_agent2::File` (content-hash `version`);
//! - findings are a **derived memoized view** keyed on `(file id, version)` — an unchanged file is
//!   a **memo hit**, never re-reviewed (v11 finding G: this is what stopped the noisy reviewer
//!   from oscillating);
//! - two passes prove it: pass 1 reviews all 5 files, pass 2 (nothing changed) is 5 memo hits.
//!
//! Honest split (see the crate `review` module docs): the LLM review is a noisy judgment with no
//! reachable fixed point, so this does NOT drive `Runtime`/`Check` to convergence — that path is
//! for deterministic diffs (`tests/reconcile.rs`, the "make cargo test green" CUJ).

use std::collections::HashMap;

use tars_agent2::review::{Backend, Finding, build_mock_service, build_real_service, dedup, review_one};
use tars_agent2::{File, World};

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

/// The review cache: `(file id, content version) → findings`. The derived "open findings" view is
/// this cache read back at each file's CURRENT version — no separate status store (v11 finding 2).
type ReviewCache = HashMap<(String, u64), Vec<Finding>>;

/// Reactively review every file NOT already cached at its current version (the map), memoizing on
/// `(id, version)`. `services` maps file id → its `LlmService`. Returns `(llm_calls, memo_hits)`.
async fn ensure_reviewed(
    world: &World,
    services: &HashMap<String, tars_pipeline::LlmService>,
    paths: &HashMap<String, std::path::PathBuf>,
    cache: &mut ReviewCache,
) -> (u32, u32) {
    let worklist: Vec<(String, u64)> = world.components().map(|c| (c.id(), c.version())).collect();

    let mut to_review: Vec<(String, u64, String)> = Vec::new();
    let mut memo_hits = 0u32;
    for (id, version) in worklist {
        if cache.contains_key(&(id.clone(), version)) {
            memo_hits += 1; // memo hit — seen at this exact content, not re-reviewed
            continue;
        }
        let content = std::fs::read_to_string(&paths[&id]).unwrap_or_default();
        to_review.push((id, version, content));
    }

    // MAP: review the misses independently and concurrently, each through its own LlmService.
    let futs = to_review.iter().map(|(id, _v, content)| {
        let service = &services[id];
        async move { review_one(service, id, content).await }
    });
    let results = futures::future::join_all(futs).await;

    // REDUCE into the cache, keyed per file version.
    let llm_calls = to_review.len() as u32;
    for ((id, version, _content), findings) in to_review.into_iter().zip(results) {
        cache.insert((id, version), findings);
    }
    (llm_calls, memo_hits)
}

/// The derived view: all cached findings for each file's current version, deduped.
fn derived_findings(world: &World, cache: &ReviewCache) -> Vec<Finding> {
    let mut out = Vec::new();
    for c in world.components() {
        if let Some(findings) = cache.get(&(c.id(), c.version())) {
            out.extend(findings.iter().cloned());
        }
    }
    dedup(out)
}

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

    // Build one LlmService per file: real = a shared DeepSeek service; mock = a per-file canned
    // service — both go through the same LlmService call path.
    let backend = Backend::detect();
    let mut services: HashMap<String, tars_pipeline::LlmService> = HashMap::new();
    match build_real_service() {
        Some(service) => {
            for (name, _) in SAMPLE {
                services.insert((*name).to_string(), service.clone());
            }
        }
        None => {
            for (name, _) in SAMPLE {
                services.insert(
                    (*name).to_string(),
                    build_mock_service(format!("mock-{name}"), mock_findings_json(name)),
                );
            }
        }
    }

    eprintln!("agent-2 review demo — provider: {}", backend.label());
    eprintln!("model: {}   files: {}", tars_agent2::review::MODEL, SAMPLE.len());

    let mut cache: ReviewCache = HashMap::new();

    // Pass 1 — every file is unseen → reviewed (the map).
    let (calls1, hits1) = ensure_reviewed(&world, &services, &paths, &mut cache).await;
    eprintln!("  [pass 1] llm reviews: {calls1}   memo hits: {hits1}");

    // Pass 2 — nothing changed → every file is a memo hit (0 llm calls). Proves the seen@version
    // memo (a re-review of unchanged content is never re-sampled).
    let (calls2, hits2) = ensure_reviewed(&world, &services, &paths, &mut cache).await;
    eprintln!("  [pass 2] llm reviews: {calls2}   memo hits: {hits2}  (unchanged files reused)");

    let findings = derived_findings(&world, &cache);
    eprintln!("  findings: {} across {} files\n", findings.len(), SAMPLE.len());

    // THE frontend contract: structured findings JSON on stdout (logs went to stderr).
    let json = serde_json::to_string_pretty(&findings).expect("serialize findings");
    println!("{json}");

    let _ = std::fs::remove_dir_all(&dir);
}
