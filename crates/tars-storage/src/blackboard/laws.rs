//! The five-laws conformance suite (Doc 19 §4.1), in tars — proof the laws are
//! the FRAMEWORK's contract, independent of any consumer and of the storage.
//!
//! `ToyStore` stands in for a real consumer: it implements [`BlackboardDomain`]
//! (the projection) AND [`BlackboardStore`] (storage over its OWN toy SQLite
//! tables — exactly how A.R.C.'s `FindingStore` reuses `findings`). Each law is
//! a GENERIC test over `dyn Blackboard<Entity=Item, Event=Ev>`, run against BOTH
//! [`SqliteBlackboard<ToyStore>`] (the orchestrator + injected store) AND
//! [`InMemoryBlackboard<ToyStore>`] (framework storage + the same domain). Same
//! assertions, two backings: that is the proof.

use rusqlite::{params, Connection};

use super::{
    BbError, Blackboard, BlackboardDomain, BlackboardStore, InMemoryBlackboard, Scope,
    SqliteBlackboard, Transition,
};

const RUN: &str = "RUN-1";

#[derive(Clone, PartialEq, Debug)]
struct Item {
    id: String,
    label: String,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Ev {
    Open,
    Close,
    Reopen,
}

struct ToyStore;

impl BlackboardDomain for ToyStore {
    type Entity = Item;
    type Event = Ev;

    fn key(e: &Item) -> String {
        e.id.clone()
    }
    fn initial_status(_e: &Item) -> String {
        "open".to_string()
    }
    fn event_str(ev: Ev) -> String {
        match ev {
            Ev::Open => "open",
            Ev::Close => "close",
            Ev::Reopen => "reopen",
        }
        .to_string()
    }
    fn event_from_str(s: &str) -> Ev {
        match s {
            "close" => Ev::Close,
            "reopen" => Ev::Reopen,
            _ => Ev::Open,
        }
    }
    fn project_status(timeline: &[Ev]) -> Option<String> {
        // Latest-wins; a bare Open sighting doesn't project.
        let mut status = None;
        for ev in timeline {
            match ev {
                Ev::Open => {}
                Ev::Close => status = Some("closed".to_string()),
                Ev::Reopen => status = Some("open".to_string()),
            }
        }
        status
    }
}

impl BlackboardStore for ToyStore {
    fn init(conn: &Connection) -> Result<(), BbError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS toy_entities (
                 id TEXT PRIMARY KEY, label TEXT NOT NULL, status TEXT NOT NULL,
                 first_seen_run TEXT NOT NULL, last_seen_run TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS toy_events (
                 id TEXT NOT NULL, run TEXT NOT NULL, kind TEXT NOT NULL, at INTEGER NOT NULL,
                 PRIMARY KEY (id, run, kind));",
        )?;
        Ok(())
    }

    fn upsert(conn: &Connection, e: &Item) -> Result<(), BbError> {
        conn.execute(
            "INSERT INTO toy_entities (id, label, status, first_seen_run, last_seen_run)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET label = excluded.label, last_seen_run = excluded.last_seen_run",
            params![e.id, e.label, Self::initial_status(e), RUN],
        )?;
        Ok(())
    }

    fn append_event(
        conn: &Connection,
        e: &Item,
        run: &str,
        ev: Ev,
        at: i64,
        _version: Option<&str>,
        _reason: Option<&str>,
    ) -> Result<bool, BbError> {
        let n = conn.execute(
            "INSERT INTO toy_events (id, run, kind, at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id, run, kind) DO NOTHING",
            params![e.id, run, Self::event_str(ev), at],
        )?;
        Ok(n > 0)
    }

    fn read_timeline(conn: &Connection, key: &str) -> Result<Vec<Ev>, BbError> {
        let mut stmt =
            conn.prepare("SELECT kind FROM toy_events WHERE id = ?1 ORDER BY at ASC, rowid ASC")?;
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
            conn.execute("UPDATE toy_entities SET status = ?2 WHERE id = ?1", params![key, status])?;
        }
        Ok(())
    }

    fn view(conn: &Connection, scope: &Scope) -> Result<Vec<Item>, BbError> {
        let mut items = Vec::new();
        let mut push = |stmt: &mut rusqlite::Statement, p: &[&dyn rusqlite::ToSql]| -> Result<(), BbError> {
            let rows = stmt.query_map(p, |r| Ok(Item { id: r.get(0)?, label: r.get(1)? }))?;
            for r in rows {
                items.push(r?);
            }
            Ok(())
        };
        match scope {
            Scope::All => {
                let mut stmt = conn.prepare("SELECT id, label FROM toy_entities ORDER BY id")?;
                push(&mut stmt, &[])?;
            }
            Scope::WithStatus(statuses) => {
                for st in statuses {
                    let mut stmt = conn
                        .prepare("SELECT id, label FROM toy_entities WHERE status = ?1 ORDER BY id")?;
                    push(&mut stmt, &[st])?;
                }
            }
            Scope::FirstSeenIn(run) => {
                let mut stmt = conn.prepare(
                    "SELECT id, label FROM toy_entities WHERE first_seen_run = ?1 ORDER BY id",
                )?;
                push(&mut stmt, &[run])?;
            }
        }
        Ok(items)
    }
}

type ToyBoard = dyn Blackboard<Entity = Item, Event = Ev>;

fn item(id: &str) -> Item {
    Item { id: id.into(), label: format!("label-{id}") }
}
fn tr(kind: Ev) -> Transition<Ev> {
    Transition::new(kind, 100, Some("v1".into()))
}

fn run_all_laws(make: &dyn Fn() -> Box<ToyBoard>) {
    // law 1 — append-only
    {
        let bb = make();
        bb.commit(&item("a"), tr(Ev::Open)).unwrap();
        bb.commit(&item("a"), tr(Ev::Close)).unwrap();
        assert_eq!(bb.timeline("a").unwrap(), vec![Ev::Open, Ev::Close], "append-only");
    }
    // law 2 — atomic: value + event both present after one commit
    {
        let bb = make();
        bb.commit(&item("b"), tr(Ev::Close)).unwrap();
        assert!(bb.view(&Scope::All).unwrap().iter().any(|x| x.id == "b"));
        assert!(!bb.timeline("b").unwrap().is_empty());
    }
    // law 3 — idempotent on (key, run, kind)
    {
        let bb = make();
        bb.commit(&item("c"), tr(Ev::Close)).unwrap();
        bb.commit(&item("c"), tr(Ev::Close)).unwrap();
        bb.commit(&item("c"), tr(Ev::Close)).unwrap();
        assert_eq!(bb.timeline("c").unwrap(), vec![Ev::Close], "idempotent: no duplicate");
    }
    // law 4 — read-your-writes (by status)
    {
        let bb = make();
        bb.commit(&item("d"), tr(Ev::Close)).unwrap();
        let seen = bb.view(&Scope::WithStatus(vec!["closed".into()])).unwrap();
        assert!(seen.iter().any(|x| x.id == "d"), "read-your-writes");
    }
    // law 5 — value ≡ timeline (open→close→reopen ⇒ open)
    {
        let bb = make();
        for k in [Ev::Open, Ev::Close, Ev::Reopen] {
            bb.commit(&item("e"), tr(k)).unwrap();
        }
        assert!(bb.view(&Scope::WithStatus(vec!["open".into()])).unwrap().iter().any(|x| x.id == "e"));
        assert!(!bb.view(&Scope::WithStatus(vec!["closed".into()])).unwrap().iter().any(|x| x.id == "e"));
    }
}

#[test]
fn sqlite_orchestrator_with_injected_store_honors_the_five_laws() {
    run_all_laws(&|| Box::new(SqliteBlackboard::<ToyStore>::in_memory(RUN).unwrap()));
}

#[test]
fn in_memory_backing_honors_the_five_laws() {
    run_all_laws(&|| Box::new(InMemoryBlackboard::<ToyStore>::new(RUN)));
}

#[test]
fn entities_persist_across_runs_same_key_two_events() {
    let conn = std::sync::Arc::new(std::sync::Mutex::new(
        rusqlite::Connection::open_in_memory().unwrap(),
    ));
    let r1 = SqliteBlackboard::<ToyStore>::open(conn.clone(), "run-1").unwrap();
    r1.commit(&item("x"), tr(Ev::Open)).unwrap();
    let r2 = SqliteBlackboard::<ToyStore>::open(conn, "run-2").unwrap();
    r2.commit(&item("x"), tr(Ev::Close)).unwrap();
    assert_eq!(r2.timeline("x").unwrap(), vec![Ev::Open, Ev::Close], "cross-run timeline accrues");
}
