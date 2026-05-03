//! Recovery-from-checkpoint integration test (Doc 04 §3.2).
//!
//! The whole point of an event-sourced trajectory is that a crash
//! between any two events leaves the state recoverable: re-open the
//! event store, replay, resume. If we never test the close→reopen
//! path end-to-end at the runtime layer, we'd be testing serialization
//! in isolation but missing the wiring bug where (e.g.) a
//! `LocalRuntime` constructed against a freshly-opened
//! `SqliteEventStore` returns surprising things on `replay`.
//!
//! tars-storage already has its own close-and-reopen test at the
//! event-store layer; this test layers on top to prove the same
//! property holds when `AgentEvent` is the unit of work.

use std::sync::Arc;
use tempfile::TempDir;

use tars_runtime::{AgentEvent, LocalRuntime, Runtime, StepIdempotencyKey};
use tars_storage::{open_event_store_at_path, EventStore};
use tars_types::{ProviderId, TrajectoryId, Usage};

fn build_runtime(dir: &TempDir) -> Arc<LocalRuntime> {
    let store: Arc<dyn EventStore> = open_event_store_at_path(&dir.path().join("events.sqlite"))
        .expect("event store opens");
    LocalRuntime::new(store)
}

#[tokio::test]
async fn trajectory_survives_runtime_restart() {
    let dir = tempfile::tempdir().unwrap();

    // ── Phase 1: write events through one runtime instance ──────────
    let traj: TrajectoryId = {
        let rt = build_runtime(&dir);
        let traj = rt.create_trajectory(None, "first-pass").await.unwrap();

        let key = StepIdempotencyKey::compute(&traj, 1, "summarise");
        rt.append(
            &traj,
            AgentEvent::StepStarted {
                traj: traj.clone(),
                step_seq: 1,
                agent: "orchestrator".into(),
                idempotency_key: key,
                input_summary: "summarise".into(),
            },
        )
        .await
        .unwrap();
        rt.append(
            &traj,
            AgentEvent::LlmCallCaptured {
                traj: traj.clone(),
                step_seq: 1,
                provider: ProviderId::new("openai_main"),
                prompt_summary: "system + user".into(),
                response_summary: "...partial...".into(),
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 4,
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
        // Crash here — no StepCompleted, no TrajectoryCompleted.
        traj
        // `rt` drops at end of scope → SQLite connection closes →
        // WAL flushes on next open.
    };

    // ── Phase 2: fresh runtime against the same file ────────────────
    let rt2 = build_runtime(&dir);

    // Recovery scan finds the trajectory.
    let listed = rt2.list_trajectories().await.unwrap();
    assert!(listed.contains(&traj), "list_trajectories sees the recovered id");

    // Replay returns every event in order.
    let events = rt2.replay(&traj).await.unwrap();
    assert_eq!(events.len(), 3, "Started + StepStarted + LlmCallCaptured");
    assert!(matches!(events[0], AgentEvent::TrajectoryStarted { .. }));
    assert!(matches!(events[1], AgentEvent::StepStarted { .. }));
    assert!(matches!(events[2], AgentEvent::LlmCallCaptured { .. }));

    // Trajectory is NOT terminated — Phase 1 never wrote Completed.
    assert!(!rt2.is_terminated(&traj).await.unwrap());

    // ── Phase 3: continue the trajectory through the new runtime ────
    rt2.append(
        &traj,
        AgentEvent::StepCompleted {
            traj: traj.clone(),
            step_seq: 1,
            output_summary: "done".into(),
            usage: Usage {
                input_tokens: 12,
                output_tokens: 18,
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    rt2.append(
        &traj,
        AgentEvent::TrajectoryCompleted {
            traj: traj.clone(),
            summary: "summarised after restart".into(),
        },
    )
    .await
    .unwrap();

    // Now terminated, full event log present.
    assert!(rt2.is_terminated(&traj).await.unwrap());
    let final_events = rt2.replay(&traj).await.unwrap();
    assert_eq!(final_events.len(), 5);
    match final_events.last().unwrap() {
        AgentEvent::TrajectoryCompleted { summary, .. } => {
            assert_eq!(summary, "summarised after restart");
        }
        other => panic!("expected TrajectoryCompleted as terminal event, got {other:?}"),
    }
}

#[tokio::test]
async fn replay_since_skips_recovered_prefix() {
    // Recovery code that's already processed up to seq=N should be
    // able to ask for "everything after N" without re-reading the
    // prefix. This is the incremental-tailing primitive.
    let dir = tempfile::tempdir().unwrap();
    let traj = {
        let rt = build_runtime(&dir);
        let t = rt.create_trajectory(None, "t").await.unwrap();
        for i in 0..5 {
            rt.append(
                &t,
                AgentEvent::TrajectorySuspended {
                    traj: t.clone(),
                    reason: format!("checkpoint-{i}"),
                },
            )
            .await
            .unwrap();
        }
        t
    };

    let rt2 = build_runtime(&dir);
    // Total 6 events (Started + 5 Suspended). After-seq=4 → 2 events.
    let tail = rt2.replay_since(&traj, 4).await.unwrap();
    assert_eq!(tail.len(), 2);
    for ev in &tail {
        assert!(matches!(ev, AgentEvent::TrajectorySuspended { .. }));
    }
}
