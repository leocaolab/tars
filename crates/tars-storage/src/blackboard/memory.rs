//! The in-memory backing of the [`Blackboard`] model — **generic** over a
//! [`BlackboardCodec`]. A real, usable backing for ephemeral / test runs, and
//! the proof the five laws are the model's contract, not SQLite's: the law tests
//! run against this AND [`super::SqliteBlackboard`] and must pass identically.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Mutex;

use super::{BbError, Blackboard, BlackboardCodec, Scope, Transition};

/// In-memory blackboard, scoped to one run (`run_id`), generic over the
/// consumer's [`BlackboardCodec`].
pub struct MemBlackboard<C: BlackboardCodec> {
    run_id: String,
    state: Mutex<State<C>>,
}

struct State<C: BlackboardCodec> {
    /// Current value per key (status is a projection of `log`, law #5).
    entities: HashMap<String, Entry<C>>,
    /// The append-only timeline across all entities (law #1).
    log: Vec<Logged>,
    _codec: PhantomData<fn() -> C>,
}

struct Entry<C: BlackboardCodec> {
    value: C::Entity,
    status: String,
    first_seen_run: String,
}

struct Logged {
    key: String,
    run_id: String,
    kind_str: String,
}

impl<C: BlackboardCodec> MemBlackboard<C> {
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            state: Mutex::new(State { entities: HashMap::new(), log: Vec::new(), _codec: PhantomData }),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

impl<C: BlackboardCodec> Blackboard for MemBlackboard<C> {
    type Entity = C::Entity;
    type Event = C::Event;

    fn view(&self, scope: &Scope) -> Result<Vec<C::Entity>, BbError> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut keys: Vec<&String> = s
            .entities
            .iter()
            .filter(|(_, e)| match scope {
                Scope::All => true,
                Scope::WithStatus(statuses) => statuses.contains(&e.status),
                Scope::FirstSeenIn(run) => &e.first_seen_run == run,
            })
            .map(|(k, _)| k)
            .collect();
        keys.sort();
        Ok(keys.into_iter().map(|k| s.entities[k].value.clone()).collect())
    }

    fn timeline(&self, key: &str) -> Result<Vec<C::Event>, BbError> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Ok(s.log
            .iter()
            .filter(|e| e.key == key)
            .map(|e| C::event_from_str(&e.kind_str))
            .collect())
    }

    fn commit(&self, e: &C::Entity, t: Transition<C::Event>) -> Result<(), BbError> {
        let key = C::key(e);
        // Law #2 (atomic): one lock guards value-set AND event-append.
        let mut s = self.state.lock().unwrap_or_else(|st| st.into_inner());

        // Upsert the value; preserve first_seen_run on re-touch.
        let first_seen = s
            .entities
            .get(&key)
            .map(|prior| prior.first_seen_run.clone())
            .unwrap_or_else(|| self.run_id.clone());
        s.entities.insert(
            key.clone(),
            Entry { value: e.clone(), status: C::initial_status(e), first_seen_run: first_seen },
        );

        // Law #3 (idempotent on key+run+kind): absorb a duplicate transition.
        let kind_str = C::event_str(t.kind);
        let dup = s
            .log
            .iter()
            .any(|ev| ev.key == key && ev.run_id == self.run_id && ev.kind_str == kind_str);
        if !dup {
            s.log.push(Logged { key: key.clone(), run_id: self.run_id.clone(), kind_str });
        }

        // Law #5 (value ≡ timeline): re-derive status from the (post-append)
        // timeline via the SHARED codec projection.
        let timeline: Vec<C::Event> = s
            .log
            .iter()
            .filter(|ev| ev.key == key)
            .map(|ev| C::event_from_str(&ev.kind_str))
            .collect();
        if let Some(status) = C::project_status(&timeline) {
            if let Some(entry) = s.entities.get_mut(&key) {
                entry.status = status;
            }
        }
        Ok(())
    }
}
