//! [`InMemoryBlackboard`] — the in-memory implementation of [`Blackboard`].
//! It needs only the [`BlackboardDomain`] seam (key / event-map / status
//! projection); its storage is a generic `HashMap` + append-only `Vec` the
//! framework owns. A real, usable backing for ephemeral / test runs, and the
//! proof the five laws are the model's contract, not SQLite's — the law tests
//! run against this AND [`super::SqliteBlackboard`] and must pass identically.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Mutex;

use super::{BbError, Blackboard, BlackboardDomain, Scope, Transition};

/// In-memory blackboard, scoped to one run, generic over a [`BlackboardDomain`].
pub struct InMemoryBlackboard<D: BlackboardDomain> {
    run_id: String,
    state: Mutex<State<D>>,
}

struct State<D: BlackboardDomain> {
    entities: HashMap<String, Entry<D>>,
    log: Vec<Logged>,
    _d: PhantomData<fn() -> D>,
}

struct Entry<D: BlackboardDomain> {
    value: D::Entity,
    status: String,
    first_seen_run: String,
}

struct Logged {
    key: String,
    run_id: String,
    kind_str: String,
}

impl<D: BlackboardDomain> InMemoryBlackboard<D> {
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            state: Mutex::new(State { entities: HashMap::new(), log: Vec::new(), _d: PhantomData }),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

impl<D: BlackboardDomain> Blackboard for InMemoryBlackboard<D> {
    type Entity = D::Entity;
    type Event = D::Event;

    fn view(&self, scope: &Scope) -> Result<Vec<D::Entity>, BbError> {
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
        // Stamp the projected status back onto each returned value, so the
        // in-memory view agrees with the timeline (law #5) — the same status a
        // SQLite store would read from its synced column.
        Ok(keys
            .into_iter()
            .map(|k| {
                let entry = &s.entities[k];
                D::with_status(&entry.value, &entry.status)
            })
            .collect())
    }

    fn timeline(&self, key: &str) -> Result<Vec<D::Event>, BbError> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Ok(s.log
            .iter()
            .filter(|e| e.key == key)
            .map(|e| D::event_from_str(&e.kind_str))
            .collect())
    }

    fn commit(&self, e: &D::Entity, t: Transition<D::Event>) -> Result<(), BbError> {
        let key = D::key(e);
        // Law #2 (atomic): one lock guards value-set AND event-append.
        let mut s = self.state.lock().unwrap_or_else(|st| st.into_inner());

        let first_seen = s
            .entities
            .get(&key)
            .map(|p| p.first_seen_run.clone())
            .unwrap_or_else(|| self.run_id.clone());
        s.entities.insert(
            key.clone(),
            Entry { value: e.clone(), status: D::initial_status(e), first_seen_run: first_seen },
        );

        // Law #3 (idempotent on key+run+kind): absorb a duplicate transition.
        let kind_str = D::event_str(t.kind);
        let dup = s
            .log
            .iter()
            .any(|ev| ev.key == key && ev.run_id == self.run_id && ev.kind_str == kind_str);
        if !dup {
            s.log.push(Logged { key: key.clone(), run_id: self.run_id.clone(), kind_str });
        }

        // Law #5 (value ≡ timeline): re-derive status from the post-append log.
        let timeline: Vec<D::Event> = s
            .log
            .iter()
            .filter(|ev| ev.key == key)
            .map(|ev| D::event_from_str(&ev.kind_str))
            .collect();
        if let Some(status) = D::project_status(&timeline) {
            if let Some(entry) = s.entities.get_mut(&key) {
                entry.status = status;
            }
        }
        Ok(())
    }
}
