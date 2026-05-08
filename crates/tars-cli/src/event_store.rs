//! Shared resolution + opening of the SQLite event store used by
//! both `tars run` (writes a 1-trajectory log per invocation) and
//! `tars trajectory ...` (reads the log back). Keeps the path
//! resolution rules in one place so the two subcommands can't
//! drift.
//!
//! Resolution order:
//!   1. `--events-path <PATH>` flag (or `TARS_EVENTS_PATH` env)
//!   2. `dirs::data_dir()/tars/events.sqlite` (XDG-aware default)
//!   3. None on platforms with no XDG-equivalent data dir
//!
//! "data_dir" not "cache_dir" — events are durable user history
//! ("here's what `tars run` did last Tuesday"), not regenerable
//! cache. Same distinction `tars-storage::default_personal_event_
//! store_path` already encodes; we re-export the resolution here so
//! the CLI doesn't need to know which crate to import from.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tars_storage::{SqliteEventStore, open_event_store_at_path};

pub fn resolve_path(explicit: Option<&std::path::Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    tars_storage::default_personal_event_store_path()
}

/// Open the event store at the resolved path, returning `None` if no
/// path could be resolved (rare — only platforms with no XDG-style
/// data dir, e.g. some embedded targets). Caller decides how to
/// degrade.
///
/// Failures to open / migrate surface as `Err` rather than `Ok(None)`
/// because at that point the user did configure a path; failing
/// silently would mask their misconfiguration.
pub fn open(explicit: Option<&std::path::Path>) -> Result<Option<Arc<SqliteEventStore>>> {
    let Some(path) = resolve_path(explicit) else {
        return Ok(None);
    };
    let store = open_event_store_at_path(&path)
        .with_context(|| format!("opening event store at {}", path.display()))?;
    Ok(Some(store))
}
