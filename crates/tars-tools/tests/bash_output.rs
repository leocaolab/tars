//! Runs alone in its own test binary, so the process's memory is this test's.

#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::json;
use tars_tools::builtins::BashTool;
use tars_tools::{Tool, ToolEnv};
use tokio_util::sync::CancellationToken;

/// Returns this process's resident set size in bytes, as `ps` reports it.
fn rss() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap()
        * 1024
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_that_writes_300_mb_costs_the_host_kilobytes_not_its_output() {
    const WRITTEN: u64 = 300_000_000;
    let before = rss();
    let peak = Arc::new(AtomicU64::new(before));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, done) = (peak.clone(), done.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                peak.fetch_max(rss(), Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let r = BashTool::new()
        .execute(
            json!({ "command": format!("head -c {WRITTEN} /dev/zero") }),
            &ToolEnv::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    done.store(true, Ordering::SeqCst);
    sampler.join().unwrap();
    let grew = peak.load(Ordering::SeqCst).saturating_sub(before);

    assert!(
        r.content.len() < 64 * 1024,
        "{} bytes returned",
        r.content.len()
    );
    assert!(
        r.content.contains("bytes truncated]"),
        "{}",
        &r.content[..80]
    );
    assert!(grew < 64 << 20, "the host's memory grew by {grew} bytes");
}
