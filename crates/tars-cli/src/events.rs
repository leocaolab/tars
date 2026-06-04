//! `tars events` — inspect the **pipeline event store** written by
//! `EventEmitterMiddleware` (Doc 17). Distinct from `tars trajectory`,
//! which reads trajectory `AgentEvent` logs from a different file.
//!
//! Subcommands:
//!
//! - `tars events list [--tenant X] [--since 1d] [--tag T] [--limit N]`
//!   — one-line-per-event summary (timestamp, tenant, model, result).
//! - `tars events show <event_id> [--with-bodies]` — full JSON payload;
//!   `--with-bodies` resolves request_ref / response_ref against the
//!   body store and prints the bytes.
//! - `tars events reasons [--tenant X] [--since 1d] [--tag T] [--json]`
//!   — aggregate validation-reject reasons by `kind` over the window
//!   ("which reason fired most"), reading the `validation_reason`
//!   (B-20.v2) off each event. The cohort view that turns the persisted
//!   reject detail into an answerable question.
//!
//! Default `--store-dir` is `~/.tars/events/`; matches the convention
//! `Pipeline.from_default(event_store_dir=...)` uses.
//!
//! These are diagnostic tools, not part of the typed API. For
//! programmatic access (tests / dogfood scripts), open the SQLite
//! files directly or use `tars_storage::SqlitePipelineEventStore`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use tars_storage::{
    BodyStore, PipelineEventQuery, PipelineEventStore, SqliteBodyStore, SqliteBodyStoreConfig,
    SqlitePipelineEventStore, SqlitePipelineEventStoreConfig,
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
    /// Aggregate validation-reject reasons by kind — "which reason
    /// fired most" over a window. Reads `validation_reason` (B-20.v2)
    /// off the events; an empty result means no rejects in the window.
    Reasons(ReasonsArgs),
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
struct ReasonsArgs {
    /// Filter by tenant id.
    #[arg(long)]
    tenant: Option<String>,
    /// Look back this far. Accepts `1d`, `2h`, `30m`, `45s`. Default
    /// `7d`. Pass `--since all` to disable the lower bound.
    #[arg(long, default_value = "7d")]
    since: String,
    /// Filter by cohort tag — only count rejects on events whose `tags`
    /// array contains this string. Lets you scope "which reason fired"
    /// to one dogfood cohort.
    #[arg(long)]
    tag: Option<String>,
    /// Cap on events scanned (the store caps any query at 10_000 too).
    #[arg(long, default_value_t = 10_000)]
    scan_limit: u32,
    /// Output as JSON (array of {kind, count, share, sample}) instead of
    /// a human-readable table.
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
    if !dir.is_dir() {
        anyhow::bail!(
            "event store path is not a directory: {}\n\
             Expected a directory containing `pipeline_events.db` + `bodies.db`.",
            dir.display()
        );
    }
    let events = open_events(&dir)?;
    let bodies = open_bodies(&dir)?;

    match args.command {
        EventsCommand::List(a) => list(&*events, a).await,
        EventsCommand::Show(a) => show(&*events, &*bodies, a).await,
        EventsCommand::Reasons(a) => reasons(&*events, a).await,
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
    Ok(SqlitePipelineEventStore::open(
        SqlitePipelineEventStoreConfig::new(path),
    )?)
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
    // Tag filter is applied in-process (no SQL pushdown yet; that's v2).
    // When a tag is set we must NOT push `--limit` down to the query —
    // doing so would cap the *scan window* to N rows and then filter,
    // so `--limit 50` could yield far fewer than 50 matches. Instead we
    // scan the store's default window (capped at 10_000 by the store),
    // filter by tag, then truncate to `--limit`, giving the expected
    // "up to N matching events" semantics. Without a tag the limit is a
    // direct SQL bound, which is exact.
    let q = PipelineEventQuery {
        tenant_id: args.tenant.map(TenantId::new),
        since,
        until: None,
        limit: if args.tag.is_some() {
            None
        } else {
            Some(args.limit)
        },
    };
    let mut events = store.query(&q).await?;
    if let Some(tag) = &args.tag {
        events.retain(|ev| match ev {
            PipelineEvent::LlmCallFinished(e) => e.tags.iter().any(|t| t == tag),
            PipelineEvent::EvaluationScored(e) => e.tags.iter().any(|t| t == tag),
            _ => false,
        });
        events.truncate(args.limit as usize);
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
    // The store exposes no by-id lookup, so we scan. The store caps any
    // single query at 10_000 rows, so an event older than the most recent
    // 10_000 is unreachable here — a documented v1 limitation (a real
    // by-id index is the v2 fix). Make `limit` explicit and surface the
    // cap in the not-found message so the failure isn't mistaken for a
    // genuinely missing event.
    const SCAN_LIMIT: u32 = 10_000;
    let all = store
        .query(&PipelineEventQuery {
            limit: Some(SCAN_LIMIT),
            ..Default::default()
        })
        .await?;
    let scanned = all.len();
    let target = all
        .into_iter()
        .find(|ev| match ev {
            PipelineEvent::LlmCallFinished(e) => e.event_id.to_string() == args.event_id,
            PipelineEvent::EvaluationScored(e) => e.event_id.to_string() == args.event_id,
            _ => false,
        })
        .with_context(|| {
            if scanned >= SCAN_LIMIT as usize {
                format!(
                    "event_id {} not found in the most recent {SCAN_LIMIT} events \
                     (older events are not scannable in v1; narrow with `tars events list --since`)",
                    args.event_id
                )
            } else {
                format!("event_id {} not found", args.event_id)
            }
        })?;

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

/// One row of the `tars events reasons` aggregation.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct ReasonStat {
    /// `ValidationReason::kind()` — `json_shape` / `not_empty` /
    /// `max_length` for builtins, or the caller's `Custom` kind.
    kind: String,
    count: usize,
    /// Fraction of all rejects in the window, 0.0–1.0.
    share: f64,
    /// A representative rendered message for this kind (first seen).
    sample: String,
}

/// Pure aggregation core — group rejects by reason kind, count, and
/// attach a sample message. Returns `(total_rejects, stats)` sorted by
/// count desc (kind asc as a stable tie-break). Split out from the
/// printing so it can be unit-tested without a store or stdout.
fn aggregate_reasons(events: &[PipelineEvent]) -> (usize, Vec<ReasonStat>) {
    use std::collections::HashMap;
    // kind -> (count, first-seen sample message)
    let mut acc: HashMap<String, (usize, String)> = HashMap::new();
    let mut total = 0usize;
    for ev in events {
        if let PipelineEvent::LlmCallFinished(e) = ev {
            if let Some(reason) = &e.validation_reason {
                total += 1;
                let entry = acc
                    .entry(reason.kind().to_string())
                    .or_insert_with(|| (0, reason.to_string()));
                entry.0 += 1;
            }
        }
    }
    let mut stats: Vec<ReasonStat> = acc
        .into_iter()
        .map(|(kind, (count, sample))| ReasonStat {
            kind,
            count,
            share: if total == 0 {
                0.0
            } else {
                count as f64 / total as f64
            },
            sample,
        })
        .collect();
    // Count desc, then kind asc so output is deterministic on ties.
    stats.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.kind.cmp(&b.kind)));
    (total, stats)
}

async fn reasons(store: &dyn PipelineEventStore, args: ReasonsArgs) -> Result<()> {
    let since = parse_since(&args.since)?;
    let q = PipelineEventQuery {
        tenant_id: args.tenant.map(TenantId::new),
        since,
        until: None,
        // A tag filter is applied in-process, so we must scan the full
        // window (not push a row cap that would truncate before
        // filtering) — same reasoning as `list`.
        limit: if args.tag.is_some() {
            None
        } else {
            Some(args.scan_limit)
        },
    };
    let mut events = store.query(&q).await?;
    if let Some(tag) = &args.tag {
        events.retain(|ev| match ev {
            PipelineEvent::LlmCallFinished(e) => e.tags.iter().any(|t| t == tag),
            _ => false,
        });
    }

    let (total, stats) = aggregate_reasons(&events);

    if args.json {
        println!("{}", serde_json::to_string(&stats)?);
        return Ok(());
    }

    if total == 0 {
        println!("(no validation rejects in window)");
        return Ok(());
    }

    println!("{total} validation reject(s) in window, by reason kind:\n");
    println!("{:<22}  {:>6}  {:>7}  sample", "kind", "count", "share");
    println!("{}", "-".repeat(80));
    for s in &stats {
        println!(
            "{:<22}  {:>6}  {:>6.1}%  {}",
            truncate(&s.kind, 22),
            s.count,
            s.share * 100.0,
            truncate(&s.sample, 44),
        );
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
    // Split off the trailing unit *character* (not byte): slicing at
    // `len()-1` would land mid-codepoint and panic on a multibyte
    // suffix like `1д`. `char_indices().last()` gives the byte offset
    // of the final char's first byte, a guaranteed UTF-8 boundary.
    let split = s.char_indices().last().map(|(i, _)| i).unwrap_or(0);
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().with_context(|| {
        format!("invalid --since value: {s} (expected like `1d`, `2h`, `30m`, `45s`, or `all`)")
    })?;
    // Saturate rather than wrap: an absurdly large value like
    // `1000000000000000000d` should clamp to "very far back", not silently
    // overflow to a tiny window in release builds.
    let secs = match unit {
        "s" => n,
        "m" => n.saturating_mul(60),
        "h" => n.saturating_mul(3600),
        "d" => n.saturating_mul(86400),
        other => anyhow::bail!("unknown duration unit `{other}`; use s/m/h/d"),
    };
    // `checked_sub` so a huge `secs` clamps to the epoch instead of
    // panicking on SystemTime underflow.
    let lower_bound = SystemTime::now()
        .checked_sub(Duration::from_secs(secs))
        .unwrap_or(UNIX_EPOCH);
    Ok(Some(lower_bound))
}

fn format_ts(t: SystemTime) -> String {
    let Ok(d) = t.duration_since(UNIX_EPOCH) else {
        // Pre-epoch timestamp — surface it explicitly rather than
        // silently fall through to "1970-01-01 00:00:00", which a user
        // can't distinguish from a legitimate epoch event.
        return "<pre-epoch>".to_string();
    };
    let secs = d.as_secs();
    // `secs` is u64; `as i64` would WRAP to a negative value for
    // secs > i64::MAX, sneaking past `from_timestamp`'s range check and
    // defeating the `<invalid>` branch below. Convert fallibly so an
    // out-of-i64-range timestamp falls straight into the invalid arm.
    let dt = i64::try_from(secs)
        .ok()
        .and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0));
    match dt {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        // Out-of-range timestamp (chrono limits, > +262kY) — same
        // principle: make corruption visible instead of silently
        // collapsing to a default-displayed value.
        None => format!("<invalid:ts={secs}>"),
    }
}

fn truncate(s: &str, n: usize) -> String {
    // Count/slice by character, not byte: a byte slice at `n-1` can land
    // mid-codepoint and panic on multibyte input (e.g. a model id with
    // non-ASCII). `char_indices` gives UTF-8-boundary offsets.
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let keep = n.saturating_sub(1);
        let end = s
            .char_indices()
            .nth(keep)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::{
        CallResult, LlmCallFinished, ProviderErrorKind, TelemetryAccumulator, Usage,
        ValidationReason, ValidationSummary,
    };
    use uuid::Uuid;

    fn reject_event(reason: ValidationReason) -> PipelineEvent {
        let tenant = TenantId::new("t");
        PipelineEvent::LlmCallFinished(Box::new(LlmCallFinished {
            event_id: Uuid::new_v4(),
            timestamp: SystemTime::now(),
            tenant_id: tenant.clone(),
            session_id: None,
            trace_id: None,
            provider_id: None,
            actual_model: "m".into(),
            request_fingerprint: [0u8; 32],
            request_ref: ContentRef::from_body(tenant, b"req"),
            has_tools: false,
            has_thinking: false,
            has_structured_output: false,
            temperature: None,
            max_output_tokens: None,
            response_ref: None,
            usage: Usage::default(),
            stop_reason: None,
            telemetry: TelemetryAccumulator::default(),
            validation_summary: ValidationSummary::default(),
            validation_reason: Some(reason),
            result: CallResult::Error {
                kind: ProviderErrorKind::ValidationFailed,
            },
            tags: vec![],
        }))
    }

    fn ok_event() -> PipelineEvent {
        match reject_event(ValidationReason::NotEmpty { field: "x".into() }) {
            PipelineEvent::LlmCallFinished(mut e) => {
                e.validation_reason = None;
                e.result = CallResult::Ok;
                PipelineEvent::LlmCallFinished(e)
            }
            other => other,
        }
    }

    #[test]
    fn aggregate_groups_by_kind_counts_and_shares() {
        let events = vec![
            reject_event(ValidationReason::NotEmpty {
                field: "text".into(),
            }),
            reject_event(ValidationReason::NotEmpty {
                field: "text".into(),
            }),
            reject_event(ValidationReason::JsonShape {
                parse_error: "boom".into(),
            }),
            reject_event(ValidationReason::Custom {
                kind: "snippet_missing".into(),
                message: "no snippet".into(),
                detail: None,
            }),
            ok_event(), // must be ignored — no reason
        ];

        let (total, stats) = aggregate_reasons(&events);
        assert_eq!(total, 4, "ok event must not be counted");
        // Sorted by count desc: not_empty(2) first, then json_shape and
        // snippet_missing (1 each, kind-asc tie-break).
        assert_eq!(stats[0].kind, "not_empty");
        assert_eq!(stats[0].count, 2);
        assert!((stats[0].share - 0.5).abs() < 1e-9);
        assert_eq!(stats[1].kind, "json_shape");
        assert_eq!(stats[2].kind, "snippet_missing");
        // Sample is the rendered Display message.
        assert!(stats[0].sample.contains("is empty"));
    }

    #[test]
    fn aggregate_empty_when_no_rejects() {
        let (total, stats) = aggregate_reasons(&[ok_event()]);
        assert_eq!(total, 0);
        assert!(stats.is_empty());
    }
}
