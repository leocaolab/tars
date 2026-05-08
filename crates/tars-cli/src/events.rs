//! `tars events` — inspect the **pipeline event store** written by
//! `EventEmitterMiddleware` (Doc 17). Distinct from `tars trajectory`,
//! which reads trajectory `AgentEvent` logs from a different file.
//!
//! Two subcommands ship in v1:
//!
//! - `tars events list [--tenant X] [--since 1d] [--tag T] [--limit N]`
//!   — one-line-per-event summary (timestamp, tenant, model, result).
//! - `tars events show <event_id> [--with-bodies]` — full JSON payload;
//!   `--with-bodies` resolves request_ref / response_ref against the
//!   body store and prints the bytes.
//!
//! Default `--store-dir` is `~/.tars/events/`; matches the convention
//! `Pipeline.from_default(event_store_dir=...)` uses.
//!
//! These are diagnostic tools, not part of the typed API. For
//! programmatic access (tests / dogfood scripts), open the SQLite
//! files directly or use `tars_storage::SqlitePipelineEventStore`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use tars_storage::{
    BodyStore, PipelineEventQuery, PipelineEventStore, SqliteBodyStore,
    SqliteBodyStoreConfig, SqlitePipelineEventStore, SqlitePipelineEventStoreConfig,
};
use tars_types::{ContentRef, PipelineEvent, TenantId};

#[derive(Args, Debug)]
pub struct EventsArgs {
    /// Path to the event store directory (containing `pipeline_events.db`
    /// + `bodies.db`). Defaults to `~/.tars/events/`.
    #[arg(long, env = "TARS_EVENT_STORE_DIR", global = true)]
    store_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: EventsCommand,
}

#[derive(Subcommand, Debug)]
enum EventsCommand {
    /// List recent events. One row per event with key columns.
    List(ListArgs),
    /// Show one event's full payload, optionally with body content.
    Show(ShowArgs),
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Filter by tenant id.
    #[arg(long)]
    tenant: Option<String>,
    /// Look back this far. Accepts `1d`, `2h`, `30m`, `45s`. Default
    /// `7d`. Pass `--since all` to disable the lower bound.
    #[arg(long, default_value = "7d")]
    since: String,
    /// Filter by cohort tag — events whose `tags` array contains this
    /// string. Repeatable; ANY match (OR semantics) is a v2 nice-to-have.
    #[arg(long)]
    tag: Option<String>,
    /// Hard cap on rows. Default 50.
    #[arg(long, default_value_t = 50)]
    limit: u32,
    /// Output as JSON lines instead of human-readable table.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ShowArgs {
    /// Event id (UUID). List with `tars events list` first.
    event_id: String,
    /// Resolve request_ref + response_ref via body store and print
    /// the body bytes (UTF-8, lossy on non-text payloads).
    #[arg(long)]
    with_bodies: bool,
}

pub async fn execute(args: EventsArgs) -> Result<()> {
    let dir = resolve_store_dir(args.store_dir.as_deref())?;
    if !dir.exists() {
        anyhow::bail!(
            "event store dir does not exist: {}\n\
             Pipeline writes to it only when constructed with `event_store_dir=...`.",
            dir.display()
        );
    }
    let events = open_events(&dir)?;
    let bodies = open_bodies(&dir)?;

    match args.command {
        EventsCommand::List(a) => list(&*events, a).await,
        EventsCommand::Show(a) => show(&*events, &*bodies, a).await,
    }
}

fn resolve_store_dir(explicit: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let home = dirs::home_dir().context("no home dir")?;
    Ok(home.join(".tars/events"))
}

fn open_events(dir: &std::path::Path) -> Result<std::sync::Arc<dyn PipelineEventStore>> {
    let path = dir.join("pipeline_events.db");
    if !path.exists() {
        anyhow::bail!(
            "pipeline_events.db not found at {}\n\
             Run a `Pipeline.complete(..., event_store_dir=...)` call first to populate it.",
            path.display()
        );
    }
    Ok(SqlitePipelineEventStore::open(SqlitePipelineEventStoreConfig::new(path))?)
}

fn open_bodies(dir: &std::path::Path) -> Result<std::sync::Arc<dyn BodyStore>> {
    let path = dir.join("bodies.db");
    if !path.exists() {
        // Bodies missing isn't fatal — events still listable.
        return Ok(SqliteBodyStore::in_memory()?);
    }
    Ok(SqliteBodyStore::open(SqliteBodyStoreConfig::new(path))?)
}

async fn list(store: &dyn PipelineEventStore, args: ListArgs) -> Result<()> {
    let since = parse_since(&args.since)?;
    let q = PipelineEventQuery {
        tenant_id: args.tenant.map(TenantId::new),
        since,
        until: None,
        limit: Some(args.limit),
    };
    let mut events = store.query(&q).await?;
    // Tag filter: post-query filter (small N; full pushdown to SQL is v2).
    if let Some(tag) = &args.tag {
        events.retain(|ev| match ev {
            PipelineEvent::LlmCallFinished(e) => e.tags.iter().any(|t| t == tag),
            PipelineEvent::EvaluationScored(e) => e.tags.iter().any(|t| t == tag),
            _ => false,
        });
    }

    if args.json {
        for ev in &events {
            println!("{}", serde_json::to_string(ev)?);
        }
        return Ok(());
    }

    if events.is_empty() {
        println!("(no events)");
        return Ok(());
    }

    println!(
        "{:<36}  {:<19}  {:<14}  {:<28}  {:<6}  tags",
        "event_id", "timestamp", "tenant", "model", "result"
    );
    println!("{}", "-".repeat(120));
    for ev in &events {
        match ev {
            PipelineEvent::LlmCallFinished(e) => {
                let ts = format_ts(e.timestamp);
                let model = truncate(&e.actual_model, 28);
                let result = match &e.result {
                    tars_types::CallResult::Ok => "ok".to_string(),
                    tars_types::CallResult::Error { kind } => format!("err:{kind}"),
                    _ => "?".to_string(),
                };
                let tags = if e.tags.is_empty() {
                    String::new()
                } else {
                    e.tags.join(",")
                };
                println!(
                    "{:<36}  {ts:<19}  {:<14}  {model:<28}  {result:<6}  {tags}",
                    e.event_id,
                    truncate(e.tenant_id.as_ref(), 14),
                );
            }
            PipelineEvent::EvaluationScored(e) => {
                let ts = format_ts(e.timestamp);
                println!(
                    "{:<36}  {ts:<19}  {:<14}  {:<28}  score   call={} score={:.3}",
                    e.event_id,
                    truncate(e.tenant_id.as_ref(), 14),
                    truncate(&e.evaluator_name, 28),
                    e.call_event_id,
                    e.score,
                );
            }
            _ => {}
        }
    }
    Ok(())
}

async fn show(
    store: &dyn PipelineEventStore,
    bodies: &dyn BodyStore,
    args: ShowArgs,
) -> Result<()> {
    let all = store
        .query(&PipelineEventQuery {
            limit: Some(10_000),
            ..Default::default()
        })
        .await?;
    let target = all
        .into_iter()
        .find(|ev| match ev {
            PipelineEvent::LlmCallFinished(e) => e.event_id.to_string() == args.event_id,
            PipelineEvent::EvaluationScored(e) => e.event_id.to_string() == args.event_id,
            _ => false,
        })
        .with_context(|| format!("event_id {} not found", args.event_id))?;

    let pretty = serde_json::to_string_pretty(&target)?;
    println!("{pretty}");

    if args.with_bodies {
        if let PipelineEvent::LlmCallFinished(e) = &target {
            print_body(bodies, &e.request_ref, "REQUEST BODY").await?;
            if let Some(rref) = &e.response_ref {
                print_body(bodies, rref, "RESPONSE BODY").await?;
            } else {
                println!("\n=== RESPONSE BODY ===\n(none — call failed)");
            }
        }
    }
    Ok(())
}

async fn print_body(bodies: &dyn BodyStore, r: &ContentRef, header: &str) -> Result<()> {
    println!("\n=== {header} ===");
    match bodies.fetch(r).await? {
        Some(bytes) => {
            // Try pretty-printing as JSON; fall back to lossy UTF-8.
            match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
                Err(_) => println!("{}", String::from_utf8_lossy(&bytes)),
            }
        }
        None => println!("(body not found in store — may have been purged)"),
    }
    Ok(())
}

fn parse_since(s: &str) -> Result<Option<SystemTime>> {
    if s == "all" {
        return Ok(None);
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num
        .parse()
        .with_context(|| format!("invalid --since value: {s} (expected like `1d`, `2h`, `30m`, `45s`, or `all`)"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        other => anyhow::bail!("unknown duration unit `{other}`; use s/m/h/d"),
    };
    Ok(Some(SystemTime::now() - Duration::from_secs(secs)))
}

fn format_ts(t: SystemTime) -> String {
    use std::time::UNIX_EPOCH;
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .unwrap_or_default();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
}
