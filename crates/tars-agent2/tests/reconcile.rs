//! End-to-end proof that the god-program's reconcile loop reaches a **fixed point** on a
//! deterministic Diff — the "make a red `<shell check>` green" CUJ, mechanically.
//!
//! The Diff is a real [`ShellCheck`] shelling out to a command whose exit code is the verdict
//! (exit-0 = Green). The effect is a real [`File`] write to disk. A stub agent stands in for the
//! LLM (so the test is hermetic — no model, no network); it reads the gap and emits the one
//! write that closes it. The loop must converge (gap empty), and it must do so *because the
//! world moved*, not because the agent claimed done.

use tars_agent2::{Agent, File, Intent, Runtime, ShellCheck, Spec, Step, View, World};

/// A deterministic stub decider: if the gap is non-empty, emit one `write` to `status.txt`
/// setting its content to `green`; otherwise propose halt. Stands in for `LlmAgent` — same
/// `Agent` contract, no LLM.
struct StubFixer {
    writes: u32,
}

#[async_trait::async_trait]
impl Agent for StubFixer {
    async fn step(&mut self, view: &View) -> Step {
        if view.gap.is_empty() {
            return Step::ProposeHalt;
        }
        self.writes += 1;
        Step::Emit(vec![Intent::new(
            "status",
            "write",
            r#"{"content":"green"}"#,
        )])
    }
}

#[tokio::test]
async fn reconcile_reaches_fixed_point_on_deterministic_check() {
    let dir = tempfile::tempdir().unwrap();
    let status_path = dir.path().join("status.txt");
    std::fs::write(&status_path, "red").unwrap();

    // Desired state: a shell check that greps the file for exactly `green`. Red until the file
    // says green. Deterministic in the on-disk state → a real fixed point exists.
    let script = format!(
        "grep -qx green {}",
        status_path.to_string_lossy()
    );
    let spec = Spec::new().with(ShellCheck::new(
        "status-green",
        "sh",
        ["-c", &script],
        dir.path(),
    ));

    let mut world = World::new().with(File::open("status", &status_path).unwrap());

    // Precondition: the world starts RED (the check fails, the gap is non-empty).
    assert!(
        !world.converged(&spec),
        "world should start red (status.txt says `red`)"
    );

    let mut agent = StubFixer { writes: 0 };
    let runtime = Runtime::new(16);
    let outcome = runtime.anneal(&mut world, &spec, &mut agent).await;

    // The loop reached the fixed point.
    assert!(
        outcome.converged(),
        "reconcile loop must converge to the fixed point, got {outcome:?}"
    );
    // And it converged because the world MOVED (the file was written), not by fiat.
    assert_eq!(
        std::fs::read_to_string(&status_path).unwrap(),
        "green",
        "the effect must have landed on disk"
    );
    assert_eq!(agent.writes, 1, "one write closed the gap");
    // Post: the deterministic check now passes.
    assert!(world.converged(&spec));
}

#[tokio::test]
async fn exhausts_fuel_honestly_when_agent_cannot_close_the_gap() {
    // An agent that emits a write that does NOT satisfy the check. The loop must NOT lie: it
    // runs out of fuel and returns the residual gap, carrying the real check detail.
    struct WrongFixer;
    #[async_trait::async_trait]
    impl Agent for WrongFixer {
        async fn step(&mut self, _view: &View) -> Step {
            Step::Emit(vec![Intent::new(
                "status",
                "write",
                r#"{"content":"still-red"}"#,
            )])
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let status_path = dir.path().join("status.txt");
    std::fs::write(&status_path, "red").unwrap();
    let script = format!("grep -qx green {}", status_path.to_string_lossy());
    let spec = Spec::new().with(ShellCheck::new("status-green", "sh", ["-c", &script], dir.path()));
    let mut world = World::new().with(File::open("status", &status_path).unwrap());

    let mut agent = WrongFixer;
    let outcome = Runtime::new(4).anneal(&mut world, &spec, &mut agent).await;
    match outcome {
        tars_agent2::Outcome::Exhausted { gap, .. } => {
            assert_eq!(gap.len(), 1, "the one check stays red");
            assert_eq!(gap.red[0].check_id, "status-green");
        }
        other => panic!("expected honest Exhausted with residual gap, got {other:?}"),
    }
}
