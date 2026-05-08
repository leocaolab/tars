//! `tars trajectory` — read-side of the runtime event log.
//!
//! Two subcommands today:
//!   `tars trajectory list`       — id + event count + terminated?
//!   `tars trajectory show <ID>`  — full event sequence as JSON lines
//!
//! Defers (no consumer yet):
//!   `tars trajectory delete <ID>`  — needs a retention policy decision
//!   `tars trajectory replay <ID>`  — needs an Agent execution loop
//!                                    (Doc 04 §4) to know what "replay"
//!                                    even means at the action level
//!
//! Output discipline mirrors `tars run`:
//!   - `list` writes a small human table to stdout (pipeable but not
//!     ideal for jq)
//!   - `show` writes one JSON object per event to stdout (one line each)
//!     so `tars trajectory show ID | jq -c 'select(.type=="step_failed")'`
//!     just works.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use tars_runtime::{LocalRuntime, Runtime};
use tars_storage::EventStore;
use tars_types::TrajectoryId;

use crate::event_store as event_store_path;

#[derive(Args, Debug)]
pub struct TrajectoryArgs {
    /// Override the event store path. Default:
    /// `$XDG_DATA_HOME/tars/events.sqlite`.
    #[arg(long, env = "TARS_EVENTS_PATH", global = true)]
    pub events_path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: TrajectoryCommand,
}

#[derive(Subcommand, Debug)]
pub enum TrajectoryCommand {
    /// List recorded trajectories (id, event count, terminal status).
    List,
    /// Dump every event for one trajectory as JSON lines to stdout.
    Show {
        /// Trajectory id (full UUID-simple form, as printed by
        /// `tars run`'s "── trajectory:" footer or by `tars trajectory list`).
        id: String,
    },
}

pub async fn execute(args: TrajectoryArgs) -> Result<()> {
    let store_arc = event_store_path::open(args.events_path.as_deref())?.context(
        "no event store available — pass --events-path or run on a platform with an XDG data dir",
    )?;
    let store: Arc<dyn EventStore> = store_arc;
    let runtime = LocalRuntime::new(store);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match args.command {
        TrajectoryCommand::List => list(&runtime, &mut out).await,
        TrajectoryCommand::Show { id } => show(&runtime, &TrajectoryId::new(id), &mut out).await,
    }
}

async fn list(runtime: &LocalRuntime, out: &mut dyn Write) -> Result<()> {
    let mut ids = runtime
        .list_trajectories()
        .await
        .context("failed to list trajectories")?;
    if ids.is_empty() {
        eprintln!("(no trajectories recorded yet)");
        return Ok(());
    }
    // Stable order so `diff <(tars trajectory list)` is meaningful.
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    writeln!(out, "{:<34} {:>6}  STATUS", "ID", "EVENTS").context("stdout write")?;
    // Audit `tars-cli-src-trajectory-1`: per-trajectory replay()
    // failures used to bail out via `?`, which meant one corrupted
    // row hid every other (working) trajectory from the user. Now
    // we render the row with a `<error>` status + log the cause to
    // stderr so a human can chase it.
    let mut had_errors = false;
    for id in &ids {
        match runtime.replay(id).await {
            Ok(events) => {
                let count = events.len();
                let status = if events
                    .last()
                    .is_some_and(tars_runtime::AgentEvent::is_terminal)
                {
                    // Distinguish completed vs abandoned at the row level
                    // — the last event's discriminator carries it.
                    match events.last().unwrap() {
                        tars_runtime::AgentEvent::TrajectoryCompleted { .. } => "completed",
                        tars_runtime::AgentEvent::TrajectoryAbandoned { .. } => "abandoned",
                        _ => "terminal",
                    }
                } else {
                    "active"
                };
                writeln!(out, "{:<34} {:>6}  {}", id.as_str(), count, status)
                    .context("stdout write")?;
            }
            Err(e) => {
                had_errors = true;
                tracing::warn!(
                    trajectory_id = %id,
                    error = %e,
                    "trajectory list: replay failed; row marked <error>",
                );
                writeln!(out, "{:<34} {:>6}  <error>", id.as_str(), "?").context("stdout write")?;
            }
        }
    }
    if had_errors {
        eprintln!(
            "(some trajectories couldn't be read — see logs above; \
             other rows still listed for visibility)"
        );
    }
    Ok(())
}

async fn show(runtime: &LocalRuntime, id: &TrajectoryId, out: &mut dyn Write) -> Result<()> {
    let events = runtime
        .replay(id)
        .await
        .context("failed to replay trajectory events")?;
    if events.is_empty() {
        anyhow::bail!(
            "no events recorded for trajectory `{}` — \
             check the id, or run `tars trajectory list` to see what's available",
            id,
        );
    }
    for (i, ev) in events.iter().enumerate() {
        let json =
            serde_json::to_string(ev).with_context(|| format!("encode event #{i} for output"))?;
        writeln!(out, "{json}").with_context(|| format!("stdout write for event #{i}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tars_runtime::AgentEvent;
    use tempfile::TempDir;

    async fn fixture(dir: &TempDir) -> (Arc<dyn EventStore>, Arc<LocalRuntime>) {
        let path = dir.path().join("events.sqlite");
        let store: Arc<dyn EventStore> = tars_storage::open_event_store_at_path(&path).unwrap();
        let rt = LocalRuntime::new(Arc::clone(&store));
        (store, rt)
    }

    #[tokio::test]
    async fn list_renders_active_completed_abandoned_rows() {
        let dir = tempfile::tempdir().unwrap();
        let (_store, rt) = fixture(&dir).await;

        let _a = rt.create_trajectory(None, "active").await.unwrap();
        let b = rt.create_trajectory(None, "to-complete").await.unwrap();
        rt.append(
            &b,
            AgentEvent::TrajectoryCompleted {
                traj: b.clone(),
                summary: "ok".into(),
            },
        )
        .await
        .unwrap();
        let c = rt.create_trajectory(None, "to-abandon").await.unwrap();
        rt.append(
            &c,
            AgentEvent::TrajectoryAbandoned {
                traj: c.clone(),
                cause: "x".into(),
            },
        )
        .await
        .unwrap();

        let mut out: Vec<u8> = Vec::new();
        list(&rt, &mut out).await.unwrap();
        let rendered = String::from_utf8(out).unwrap();

        assert!(rendered.contains("STATUS"), "header missing: {rendered}");
        assert!(
            rendered.contains(" active"),
            "active row missing: {rendered}"
        );
        assert!(
            rendered.contains(" completed"),
            "completed row missing: {rendered}"
        );
        assert!(
            rendered.contains(" abandoned"),
            "abandoned row missing: {rendered}"
        );
    }

    #[tokio::test]
    async fn list_marks_corrupted_row_as_error_without_hiding_others() {
        // Regression test for the bug documented at the top of `list()`:
        // one corrupted trajectory used to bail out via `?` and hide
        // every other row.
        let dir = tempfile::tempdir().unwrap();
        let (store, rt) = fixture(&dir).await;

        // One healthy trajectory.
        let _a = rt.create_trajectory(None, "healthy").await.unwrap();
        // One corrupted trajectory: bypass the runtime and write a
        // payload that won't deserialize back into `AgentEvent`, so
        // `replay()` returns `RuntimeError::Serde`.
        let bad_id = TrajectoryId::new("corrupted-traj-id");
        store
            .append(&bad_id, &[json!({"not": "an AgentEvent"})])
            .await
            .unwrap();

        let mut out: Vec<u8> = Vec::new();
        list(&rt, &mut out).await.unwrap();
        let rendered = String::from_utf8(out).unwrap();

        assert!(
            rendered.contains("<error>"),
            "corrupted row should render as <error>: {rendered}"
        );
        assert!(
            rendered.contains("corrupted-traj-id"),
            "corrupted id should still appear in the table: {rendered}"
        );
        assert!(
            rendered.contains(" active"),
            "healthy row should still appear despite sibling failure: {rendered}"
        );
    }

    #[tokio::test]
    async fn show_errors_for_unknown_trajectory() {
        let dir = tempfile::tempdir().unwrap();
        let (_store, rt) = fixture(&dir).await;
        let unknown = TrajectoryId::new("definitely-not-a-real-id");
        let mut out: Vec<u8> = Vec::new();
        let result = show(&rt, &unknown, &mut out).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("no events"));
        assert!(msg.contains("definitely-not-a-real-id"));
    }
}
