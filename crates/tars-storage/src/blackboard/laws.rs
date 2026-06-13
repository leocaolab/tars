//! The five-laws conformance suite (Doc 19 §4.1), in tars — proof the laws are
//! the FRAMEWORK's contract, independent of any consumer.
//!
//! A toy codec (`ToyCodec`) stands in for a real domain. Each law is a GENERIC
//! test over `dyn Blackboard<Entity=Item, Event=Ev>`, run against BOTH
//! [`SqliteBlackboard`] and [`MemBlackboard`]. Same assertions, two backings:
//! that is the proof the laws are the model, not the storage.

use super::{
    BbError, Blackboard, BlackboardCodec, MemBlackboard, Scope, SqliteBlackboard, Transition,
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

struct ToyCodec;

impl BlackboardCodec for ToyCodec {
    type Entity = Item;
    type Event = Ev;

    fn key(e: &Item) -> String {
        e.id.clone()
    }

    fn encode(e: &Item) -> Result<String, BbError> {
        Ok(format!("{}\u{1f}{}", e.id, e.label))
    }

    fn decode(s: &str) -> Result<Item, BbError> {
        let (id, label) = s
            .split_once('\u{1f}')
            .ok_or_else(|| BbError::Codec(format!("bad item: {s:?}")))?;
        Ok(Item { id: id.to_string(), label: label.to_string() })
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
        // Latest-wins; an `Open` sighting doesn't project (keeps initial).
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

type ToyBoard = dyn Blackboard<Entity = Item, Event = Ev>;

fn item(id: &str) -> Item {
    Item { id: id.into(), label: format!("label-{id}") }
}

fn tr(kind: Ev) -> Transition<Ev> {
    Transition::new(kind, 100, Some("v1".into()))
}

fn run_all_laws(make: &dyn Fn() -> Box<ToyBoard>) {
    law1_append_only(make);
    law2_atomic_both_present(make);
    law3_idempotent(make);
    law4_read_your_writes(make);
    law5_value_eq_timeline(make);
}

fn law1_append_only(make: &dyn Fn() -> Box<ToyBoard>) {
    let bb = make();
    let it = item("a");
    bb.commit(&it, tr(Ev::Open)).unwrap();
    bb.commit(&it, tr(Ev::Close)).unwrap();
    assert_eq!(
        bb.timeline("a").unwrap(),
        vec![Ev::Open, Ev::Close],
        "append-only: the prior Open must survive a later Close",
    );
}

fn law2_atomic_both_present(make: &dyn Fn() -> Box<ToyBoard>) {
    let bb = make();
    let it = item("b");
    bb.commit(&it, tr(Ev::Close)).unwrap();
    let entity_present = bb.view(&Scope::All).unwrap().iter().any(|x| x.id == "b");
    let event_present = !bb.timeline("b").unwrap().is_empty();
    assert!(entity_present && event_present, "atomic: value + event land together");
}

fn law3_idempotent(make: &dyn Fn() -> Box<ToyBoard>) {
    let bb = make();
    let it = item("c");
    bb.commit(&it, tr(Ev::Close)).unwrap();
    bb.commit(&it, tr(Ev::Close)).unwrap();
    bb.commit(&it, tr(Ev::Close)).unwrap();
    assert_eq!(
        bb.timeline("c").unwrap(),
        vec![Ev::Close],
        "idempotent: re-committing the same (key, run, kind) adds no duplicate",
    );
}

fn law4_read_your_writes(make: &dyn Fn() -> Box<ToyBoard>) {
    let bb = make();
    let it = item("d");
    bb.commit(&it, tr(Ev::Close)).unwrap();
    let seen = bb.view(&Scope::WithStatus(vec!["closed".into()])).unwrap();
    assert!(
        seen.iter().any(|x| x.id == "d"),
        "read-your-writes: a committed Close is viewable by status immediately",
    );
}

fn law5_value_eq_timeline(make: &dyn Fn() -> Box<ToyBoard>) {
    let bb = make();
    let it = item("e");
    for k in [Ev::Open, Ev::Close, Ev::Reopen] {
        bb.commit(&it, tr(k)).unwrap();
    }
    // After open→close→reopen the projection is "open"; the entity must agree.
    let in_open = bb.view(&Scope::WithStatus(vec!["open".into()])).unwrap();
    assert!(
        in_open.iter().any(|x| x.id == "e"),
        "value ≡ timeline: status follows the timeline's projection (reopen → open)",
    );
    let in_closed = bb.view(&Scope::WithStatus(vec!["closed".into()])).unwrap();
    assert!(!in_closed.iter().any(|x| x.id == "e"), "and is NOT still closed");
}

#[test]
fn sqlite_backing_honors_the_five_laws() {
    run_all_laws(&|| Box::new(SqliteBlackboard::<ToyCodec>::in_memory(RUN).unwrap()));
}

#[test]
fn in_memory_backing_honors_the_five_laws() {
    run_all_laws(&|| Box::new(MemBlackboard::<ToyCodec>::new(RUN)));
}

#[test]
fn entities_persist_across_runs_same_key_two_events() {
    // Found in run 1, closed in run 2 — same key, two events, one entity.
    let conn = std::sync::Arc::new(std::sync::Mutex::new(
        rusqlite::Connection::open_in_memory().unwrap(),
    ));
    let r1 = SqliteBlackboard::<ToyCodec>::open(conn.clone(), "run-1").unwrap();
    r1.commit(&item("x"), tr(Ev::Open)).unwrap();
    let r2 = SqliteBlackboard::<ToyCodec>::open(conn, "run-2").unwrap();
    r2.commit(&item("x"), tr(Ev::Close)).unwrap();
    assert_eq!(r2.timeline("x").unwrap(), vec![Ev::Open, Ev::Close], "cross-run timeline accrues");
    assert_eq!(r2.view(&Scope::FirstSeenIn("run-1".into())).unwrap().len(), 1, "birth run is immutable");
}
