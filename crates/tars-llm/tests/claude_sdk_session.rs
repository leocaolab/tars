//! Runs the claude_sdk provider against a stand-in daemon, in its own test binary: the child
//! reaper's tests kill every registered process group, the daemon's included.
#![cfg(all(unix, feature = "cli"))]

use std::io::Write as _;
use std::time::Duration;

use tars_llm::ProviderError;
use tars_llm::provider::LlmProvider;
use tars_llm::provider::backends::claude_sdk::ClaudeSdkProviderBuilder;
use tars_types::{Caller, ChatRequest, ProviderId};

/// A daemon that notes each start, reads every request and never answers stands in for one still
/// working on a slow reply.
#[tokio::test]
async fn a_timeout_waiting_for_the_reply_keeps_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let starts = dir.path().join("starts");
    let script = dir.path().join("daemon.sh");
    let mut f = std::fs::File::create(&script).unwrap();
    writeln!(f, "echo start >> '{}'\ncat > /dev/null", starts.display()).unwrap();
    drop(f);
    let p = ClaudeSdkProviderBuilder::new(ProviderId::new("silent_daemon"))
        .executable("sh")
        .script_path(script.to_str().unwrap().to_string())
        .timeout(Duration::from_millis(200))
        .build();

    for _ in 0..2 {
        let r = p
            .clone()
            .complete(ChatRequest::user("ping"), "m", Caller::test_default())
            .await;
        assert!(matches!(r, Err(ProviderError::TimedOut { .. })), "{r:?}");
    }

    let started = std::fs::read_to_string(&starts).unwrap().lines().count();
    assert_eq!(
        started, 1,
        "a timeout while waiting dropped the daemon session"
    );
}
