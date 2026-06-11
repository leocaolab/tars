//! `tars trajectory` — read-side of the runtime event log.
//!
//! Subcommands:
//!   `tars trajectory list`        — id + event count + terminated?
//!   `tars trajectory show <ID>`   — full event sequence as JSON lines
//!   `tars trajectory score <ID>`  — score the cross-call tool sequence
//!                                   against `--expected` (Doc 26 M2');
//!                                   non-zero exit below `--threshold`
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
use tars_runtime::{LocalRuntime, MatchMode, Runtime};
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
    /// Score a trajectory's cross-call tool sequence against an expected
    /// list (Doc 26 M2'). Exits non-zero when the score is below the
    /// threshold, so it doubles as a CI gate.
    Score {
        /// Trajectory id (as in `show`).
        id: String,
        /// Expected tool names: a comma list (`search,read_file`) or
        /// `@path.json` for a JSON array (`["search",…]` or `[{"name":…}]`).
        #[arg(long)]
        expected: String,
        /// Match mode: `exact` | `ordered` | `set` | `args`. Default
        /// `ordered`. (`args` has no effect here — recorded trajectories
        /// store tool names only, so it degrades to `exact`.)
        #[arg(long, default_value = "ordered")]
        mode: String,
        /// Pass threshold on the score (default 1.0 = strict).
        #[arg(long, default_value_t = 1.0)]
        threshold: f64,
        /// Emit a JSON object instead of the human summary.
        #[arg(long)]
        json: bool,
    },
}

pub async fn execute(args: TrajectoryArgs) -> Result<()> {
    let store_arc = event_store_path::open(args.events_path.as_deref())?.context(
        "no event store available — pass --events-path or run on a platform with an XDG data dir",
    )?;
    let store: Arc<dyn EventStore> = store_arc;
    let runtime = LocalRuntime::new(store);

    // Render into an in-memory buffer FIRST, then take the stdout lock
    // and flush it synchronously. Holding a non-Send `StdoutLock`
    // across the `.await`s in list()/show() would make this future
    // non-Send (and risks deadlock if another task touches stdout).
    // Collect → drop awaits → lock → write keeps it Send and simple.
    let mut buf: Vec<u8> = Vec::new();
    let mut score_failed = false;
    match args.command {
        TrajectoryCommand::List => list(&runtime, &mut buf).await?,
        TrajectoryCommand::Show { id } => show(&runtime, &TrajectoryId::new(id), &mut buf).await?,
        TrajectoryCommand::Score {
            id,
            expected,
            mode,
            threshold,
            json,
        } => {
            let m = MatchMode::parse(&mode).ok_or_else(|| {
                anyhow::anyhow!("unknown --mode `{mode}`. Recognized: exact, ordered, set, args")
            })?;
            let exp = parse_expected(&expected)?;
            let passed = score(
                &runtime,
                &TrajectoryId::new(id),
                &exp,
                m,
                threshold,
                json,
                &mut buf,
            )
            .await?;
            score_failed = !passed;
        }
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(&buf).context("stdout write")?;
    out.flush().context("stdout flush")?;
    // `score` below threshold = non-zero exit (CI gate) — after the flush so
    // the human/JSON result is still printed.
    if score_failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Resolve the `--expected` spec into tool names: a comma list, or `@file`
/// pointing at a JSON array of names / `{name,…}` objects.
fn parse_expected(spec: &str) -> Result<Vec<String>> {
    if let Some(path) = spec.strip_prefix('@') {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading expected-tools file `{path}`"))?;
        let arr: Vec<serde_json::Value> = serde_json::from_str(&raw)
            .with_context(|| format!("parsing `{path}` as a JSON array of tool names"))?;
        arr.iter()
            .map(|v| match v {
                serde_json::Value::String(s) => Ok(s.clone()),
                serde_json::Value::Object(o) => o
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("array element missing string `name`: {v}")),
                _ => Err(anyhow::anyhow!(
                    "array element must be a string or {{\"name\":…}}: {v}"
                )),
            })
            .collect()
    } else {
        Ok(spec
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect())
    }
}

/// Score one trajectory's cross-call tool sequence against `expected`.
/// Returns whether it passed the threshold. Writes the human/JSON report
/// to `out`.
async fn score(
    runtime: &LocalRuntime,
    id: &TrajectoryId,
    expected: &[String],
    mode: MatchMode,
    threshold: f64,
    json: bool,
    out: &mut dyn Write,
) -> Result<bool> {
    let events = runtime
        .replay(id)
        .await
        .context("failed to replay trajectory events")?;
    if events.is_empty() {
        anyhow::bail!(
            "no events recorded for trajectory `{}` — \
             check the id, or run `tars trajectory list`",
            id,
        );
    }
    let actual = tars_runtime::tool_sequence(&events);
    let a: Vec<&str> = actual.iter().map(String::as_str).collect();
    let e: Vec<&str> = expected.iter().map(String::as_str).collect();
    let s = tars_runtime::trajectory_match::score_names(&a, &e, mode);
    let passed = s >= threshold;

    if json {
        let v = serde_json::json!({
            "trajectory": id.as_str(),
            "mode": mode.as_str(),
            "threshold": threshold,
            "score": s,
            "passed": passed,
            "actual": actual,
            "expected": expected,
        });
        writeln!(out, "{}", serde_json::to_string(&v).context("encode score json")?)
            .context("stdout write")?;
    } else {
        writeln!(out, "trajectory {id}").context("stdout write")?;
        writeln!(out, "  mode:      {} (threshold {threshold:.2})", mode.as_str())
            .context("stdout write")?;
        writeln!(
            out,
            "  score:     {s:.3}  {}",
            if passed { "PASS" } else { "FAIL" }
        )
        .context("stdout write")?;
        writeln!(out, "  actual:    {actual:?}").context("stdout write")?;
        writeln!(out, "  expected:  {expected:?}").context("stdout write")?;
    }
    Ok(passed)
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

    // ── trajectory score (Doc 26 M2') ────────────────────────────────

    #[test]
    fn parse_expected_handles_comma_list_and_json_file() {
        assert_eq!(
            parse_expected("search, read_file ,").unwrap(),
            vec!["search".to_string(), "read_file".to_string()]
        );
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("e.json");
        std::fs::write(&p, r#"["a", {"name": "b"}]"#).unwrap();
        assert_eq!(
            parse_expected(&format!("@{}", p.display())).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        // malformed JSON → fail-closed
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{not array").unwrap();
        assert!(parse_expected(&format!("@{}", bad.display())).is_err());
    }

    async fn traj_with_tools(rt: &LocalRuntime, per_call: &[&[&str]]) -> TrajectoryId {
        let id = rt.create_trajectory(None, "scored").await.unwrap();
        for (i, tools) in per_call.iter().enumerate() {
            rt.append(
                &id,
                AgentEvent::LlmCallCaptured {
                    traj: id.clone(),
                    step_seq: (i + 1) as u32,
                    provider: tars_types::ProviderId::new("p"),
                    prompt_summary: "x".into(),
                    response_summary: "y".into(),
                    usage: tars_types::Usage::default(),
                    system_prompt_hash: None,
                    tool_calls: tools.iter().map(|s| s.to_string()).collect(),
                },
            )
            .await
            .unwrap();
        }
        id
    }

    #[tokio::test]
    async fn score_passes_on_matching_cross_call_sequence_and_fails_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let (_s, rt) = fixture(&dir).await;
        // Two LLM calls: [search, read_file] then [edit_file] → cross-call
        // sequence = [search, read_file, edit_file].
        let id = traj_with_tools(&rt, &[&["search", "read_file"], &["edit_file"]]).await;

        let expected = vec![
            "search".to_string(),
            "read_file".to_string(),
            "edit_file".to_string(),
        ];
        let mut out = Vec::new();
        let passed = score(&rt, &id, &expected, MatchMode::Exact, 1.0, false, &mut out)
            .await
            .unwrap();
        assert!(passed);
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("PASS"), "{rendered}");
        assert!(rendered.contains("edit_file"), "{rendered}");

        // Wrong expected under exact → fail.
        let mut out2 = Vec::new();
        let passed2 = score(
            &rt,
            &id,
            &["search".to_string()],
            MatchMode::Exact,
            1.0,
            false,
            &mut out2,
        )
        .await
        .unwrap();
        assert!(!passed2);
    }
}
