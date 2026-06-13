//! A runnable tour of the tars **blackboard framework** (Doc 19 §4.1).
//!
//! Run with: `cargo run --example blackboard_demo -p tars-storage`
//!
//! The story: a code-review board where a finding moves found → fixed →
//! verified → merged. The whole point of the blackboard is that EACH step
//! appends ITS OWN event at source, so the timeline is COMPLETE — it never
//! collapses to "only the last event" (the bug the model exists to kill).
//!
//! What this proves at the API surface:
//!
//! 1. A consumer supplies ONLY a domain + store (`ReviewBoard` here): tars owns
//!    the abstraction, the orchestration, and the five laws.
//! 2. The SAME `ReviewBoard` plugs into BOTH backings — `SqliteBlackboard`
//!    (orchestrator over an injected store) and `InMemoryBlackboard` (framework
//!    storage) — and behaves identically. The laws are the model, not the storage.
//! 3. Idempotency, value≡timeline (no-downgrade), and cross-run accrual.

use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};
use tars_storage::{
    BbError, Blackboard, BlackboardDomain, BlackboardStore, InMemoryBlackboard, Scope,
    SqliteBlackboard, Transition,
};

// ── The consumer's domain ────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Finding {
    id: String,
    title: String,
    status: String, // the current projected status, as read from the board
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Event {
    Found,
    Fixed,
    Verified,
    Merged,
    Reopened,
}

/// The consumer's binding: domain seam + a store over its OWN tables. This is
/// the entire amount of code a consumer writes — no orchestration, no laws.
struct ReviewBoard;

impl BlackboardDomain for ReviewBoard {
    type Entity = Finding;
    type Event = Event;

    fn key(e: &Finding) -> String {
        e.id.clone()
    }
    fn initial_status(_e: &Finding) -> String {
        "new".to_string()
    }
    fn event_str(ev: Event) -> String {
        match ev {
            Event::Found => "found",
            Event::Fixed => "fixed",
            Event::Verified => "verified",
            Event::Merged => "merged",
            Event::Reopened => "reopened",
        }
        .to_string()
    }
    fn event_from_str(s: &str) -> Event {
        match s {
            "fixed" => Event::Fixed,
            "verified" => Event::Verified,
            "merged" => Event::Merged,
            "reopened" => Event::Reopened,
            _ => Event::Found,
        }
    }
    /// value ≡ timeline: the fixed→verified→merged progression never downgrades
    /// (merged is the terminal state — the fix landed on main); reopened resets;
    /// a bare `found` doesn't project (keeps the initial status).
    fn project_status(timeline: &[Event]) -> Option<String> {
        let rank = |e: Event| match e {
            Event::Fixed => 1,
            Event::Verified => 2,
            Event::Merged => 3,
            _ => 0,
        };
        let name = |e: Event| ReviewBoard::event_str(e);
        let mut status: Option<String> = None;
        let mut chain = 0;
        for &ev in timeline {
            match ev {
                Event::Found => {}
                Event::Reopened => {
                    status = Some("new".into());
                    chain = 0;
                }
                e if rank(e) > 0 => {
                    if rank(e) > chain {
                        chain = rank(e);
                        status = Some(name(e));
                    }
                }
                e => {
                    status = Some(name(e));
                    chain = 0;
                }
            }
        }
        status
    }

    fn with_status(e: &Finding, status: &str) -> Finding {
        Finding { status: status.to_string(), ..e.clone() }
    }
}

impl BlackboardStore for ReviewBoard {
    fn init(conn: &Connection) -> Result<(), BbError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS review_findings (
                 id TEXT PRIMARY KEY, title TEXT NOT NULL, status TEXT NOT NULL,
                 first_seen_run TEXT NOT NULL, last_seen_run TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS review_events (
                 id TEXT NOT NULL, run TEXT NOT NULL, kind TEXT NOT NULL,
                 at INTEGER NOT NULL, version TEXT,
                 PRIMARY KEY (id, run, kind));",
        )?;
        Ok(())
    }

    fn upsert(conn: &Connection, e: &Finding) -> Result<(), BbError> {
        conn.execute(
            "INSERT INTO review_findings (id, title, status, first_seen_run, last_seen_run)
             VALUES (?1, ?2, ?3, 'run', 'run')
             ON CONFLICT(id) DO UPDATE SET title = excluded.title",
            params![e.id, e.title, Self::initial_status(e)],
        )?;
        Ok(())
    }

    fn append_event(
        conn: &Connection,
        e: &Finding,
        run: &str,
        ev: Event,
        at: i64,
        version: Option<&str>,
        _reason: Option<&str>,
    ) -> Result<bool, BbError> {
        let n = conn.execute(
            "INSERT INTO review_events (id, run, kind, at, version) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id, run, kind) DO NOTHING",
            params![e.id, run, Self::event_str(ev), at, version],
        )?;
        Ok(n > 0)
    }

    fn read_timeline(conn: &Connection, key: &str) -> Result<Vec<Event>, BbError> {
        let mut stmt = conn
            .prepare("SELECT kind FROM review_events WHERE id = ?1 ORDER BY at ASC, rowid ASC")?;
        let rows = stmt.query_map([key], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(Self::event_from_str(&r?));
        }
        Ok(out)
    }

    fn sync_status(conn: &Connection, key: &str) -> Result<(), BbError> {
        let timeline = Self::read_timeline(conn, key)?;
        if let Some(status) = Self::project_status(&timeline) {
            conn.execute(
                "UPDATE review_findings SET status = ?2 WHERE id = ?1",
                params![key, status],
            )?;
        }
        Ok(())
    }

    fn view(conn: &Connection, scope: &Scope) -> Result<Vec<Finding>, BbError> {
        let (sql, bind): (String, Option<String>) = match scope {
            Scope::All => ("SELECT id, title, status FROM review_findings ORDER BY id".into(), None),
            Scope::WithStatus(s) => (
                "SELECT id, title, status FROM review_findings WHERE status = ?1 ORDER BY id".into(),
                s.first().cloned(),
            ),
            Scope::FirstSeenIn(r) => (
                "SELECT id, title, status FROM review_findings WHERE first_seen_run = ?1 ORDER BY id"
                    .into(),
                Some(r.clone()),
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let map = |r: &rusqlite::Row| {
            Ok(Finding { id: r.get(0)?, title: r.get(1)?, status: r.get(2)? })
        };
        let rows = match bind {
            Some(b) => stmt.query_map([b], map)?.collect::<Result<Vec<_>, _>>()?,
            None => stmt.query_map([], map)?.collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn finding(id: &str, title: &str) -> Finding {
    Finding { id: id.into(), title: title.into(), status: "new".into() }
}

fn at(kind: Event, secs: i64, commit: &str) -> Transition<Event> {
    Transition::new(kind, secs, Some(commit.into()))
}

fn show(bb: &dyn Blackboard<Entity = Finding, Event = Event>, id: &str) {
    let timeline = bb.timeline(id).unwrap();
    let status = bb
        .view(&Scope::All)
        .unwrap()
        .into_iter()
        .find(|f| f.id == id)
        .map(|f| f.status)
        .unwrap_or_else(|| "<gone>".into());
    let kinds: Vec<&str> = timeline
        .iter()
        .map(|e| match e {
            Event::Found => "found",
            Event::Fixed => "fixed",
            Event::Verified => "verified",
            Event::Merged => "merged",
            Event::Reopened => "reopened",
        })
        .collect();
    println!("   {id}: status={status:<8}  timeline={kinds:?}");
}

/// The same lifecycle, run against whatever backing is handed in.
fn lifecycle(bb: &dyn Blackboard<Entity = Finding, Event = Event>, label: &str) {
    println!("\n── {label} ──");
    let f = finding("F-1", "swallowed exception");

    // Each pipeline step appends ITS OWN event — at source, with its own commit.
    bb.commit(&f, at(Event::Found, 100, "aaaa")).unwrap();
    println!(" scan   → found");
    show(bb, "F-1");
    bb.commit(&f, at(Event::Fixed, 200, "bbbb")).unwrap();
    println!(" fix    → fixed");
    show(bb, "F-1");
    bb.commit(&f, at(Event::Verified, 300, "cccc")).unwrap();
    println!(" verify → verified");
    show(bb, "F-1");
    bb.commit(&f, at(Event::Merged, 400, "dddd")).unwrap();
    println!(" merge  → merged");
    show(bb, "F-1");

    println!(" ⇒ the timeline is COMPLETE (found→fixed→verified→merged), not collapsed to just `merged`.");

    // Law #3: re-committing the same transition is absorbed (no duplicate).
    bb.commit(&f, at(Event::Merged, 400, "dddd")).unwrap();
    println!(" idempotent re-commit of `merged`:");
    show(bb, "F-1");
}

fn main() -> Result<(), BbError> {
    println!("tars blackboard — one domain (`ReviewBoard`), two backings, identical behavior\n");
    println!("The consumer writes ONLY the domain + store; tars owns the abstraction,");
    println!("the transaction/ordering orchestration, and the five laws.");

    // Backing A: the SQLite orchestrator driving the injected ReviewBoard store.
    let sqlite = SqliteBlackboard::<ReviewBoard>::in_memory("run-1")?;
    lifecycle(&sqlite, "SqliteBlackboard<ReviewBoard>  (orchestrator + injected store)");

    // Backing B: the framework's in-memory storage, same domain seam.
    let mem = InMemoryBlackboard::<ReviewBoard>::new("run-1");
    lifecycle(&mem, "InMemoryBlackboard<ReviewBoard>  (framework storage)");

    // value≡timeline + cross-run no-downgrade (SQLite, two handles share one db).
    println!("\n── value ≡ timeline across runs (no-downgrade) ──");
    let conn = Arc::new(Mutex::new(Connection::open_in_memory()?));
    let r1 = SqliteBlackboard::<ReviewBoard>::open(conn.clone(), "run-1")?;
    r1.commit(&finding("F-9", "race"), at(Event::Found, 100, "aaaa"))?;
    r1.commit(&finding("F-9", "race"), at(Event::Fixed, 200, "bbbb"))?;
    r1.commit(&finding("F-9", "race"), at(Event::Merged, 300, "cccc"))?;
    println!(" run-1 found→fixed→merged:");
    show(&r1, "F-9");
    // A LATER run re-emits a STALE `fixed` (a mirror after merge). It must NOT
    // pull the finding back from merged — the timeline fold ignores the downgrade.
    let r2 = SqliteBlackboard::<ReviewBoard>::open(conn, "run-2")?;
    r2.commit(&finding("F-9", "race"), at(Event::Fixed, 400, "dddd"))?;
    println!(" run-2 emits a stale `fixed` — status stays merged (no downgrade):");
    show(&r2, "F-9");

    println!("\nSame domain, two storages, one set of laws. tars never learns what a Finding is.");
    Ok(())
}
