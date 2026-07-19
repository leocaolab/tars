//! `review-cli <file.rs> [file2.rs …]` — the file-taking code reviewer the VSCode frontend
//! spawns. Reviews each file map-reduce through `tars_pipeline::LlmService` (real DeepSeek when
//! `DEEPSEEK_API_KEY` is set, else a mock stream — same code path) and prints a stable
//! `Finding` JSON array to **stdout**. All logs go to **stderr**, so stdout is clean JSON.
//!
//! ## Arguments
//! - File paths as args: `review-cli a.rs b.rs`
//! - Or a newline-separated list on stdin: `printf 'a.rs\nb.rs\n' | review-cli`
//!
//! ## Output (the frontend contract)
//! A JSON array of `{ "file", "line": <int|null>, "message", "status", "severity" }`. The `file`
//! field echoes the path **exactly as passed** — the VSCode extension passes absolute paths, so
//! it resolves each with `Uri.file(finding.file)` directly.
//!
//! ## Mock note
//! Without an API key, arbitrary files (whose bugs we can't know a priori) get a single
//! placeholder finding so the pipeline path still exercises end-to-end. The stderr banner says
//! which backend ran. For real findings, set `DEEPSEEK_API_KEY`.

use std::io::Read;
use std::path::PathBuf;

use tars_agent2::review::{
    Backend, Finding, build_mock_service, build_real_service, dedup, review_one,
};

/// Collect target paths: CLI args first; if none, read a newline list from stdin.
fn collect_paths() -> Vec<PathBuf> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        return args.into_iter().map(PathBuf::from).collect();
    }
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_ok() {
        return buf
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    Vec::new()
}

/// The mock placeholder for a file we can't hand-author findings for — honest about being a stub.
fn mock_placeholder_json(label: &str) -> String {
    format!(
        r#"{{"findings":[{{"line":null,"severity":"low","message":"[mock] no DEEPSEEK_API_KEY set — this is a placeholder finding for `{label}`; set the key for a real review"}}]}}"#
    )
}

#[tokio::main]
async fn main() {
    let paths = collect_paths();
    if paths.is_empty() {
        eprintln!("usage: review-cli <file.rs> [file2.rs …]   (or pipe a newline-separated list on stdin)");
        // Emit a valid (empty) JSON array so a caller that always JSON.parses stdout never breaks.
        println!("[]");
        std::process::exit(2);
    }

    let backend = Backend::detect();
    eprintln!("review-cli — provider: {}", backend.label());
    eprintln!("files: {}", paths.len());

    // Build the service(s): real = one shared DeepSeek service; mock = one per file (canned).
    let real = if backend == Backend::RealDeepSeek {
        build_real_service()
    } else {
        None
    };

    // MAP: review each file independently (concurrently). Each future reads its file's content and
    // reviews it through an LlmService (shared real, or a per-file mock).
    let futs = paths.iter().map(|path| {
        let label = path.to_string_lossy().to_string();
        let real = real.clone();
        async move {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("  [review] {label}: cannot read file: {e}");
                    return Vec::<Finding>::new();
                }
            };
            match real {
                Some(service) => review_one(&service, &label, &content).await,
                None => {
                    let service = build_mock_service(
                        format!("mock-{label}"),
                        mock_placeholder_json(&label),
                    );
                    review_one(&service, &label, &content).await
                }
            }
        }
    });
    let per_file = futures::future::join_all(futs).await;

    // REDUCE: flatten + dedup.
    let all: Vec<Finding> = per_file.into_iter().flatten().collect();
    let findings = dedup(all);
    eprintln!("findings: {}", findings.len());

    // THE frontend contract: findings JSON on stdout.
    match serde_json::to_string_pretty(&findings) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            eprintln!("failed to serialize findings: {e}");
            println!("[]");
            std::process::exit(1);
        }
    }
}
