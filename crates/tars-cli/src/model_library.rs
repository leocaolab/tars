//! The persisted **model library** for `tars models`.
//!
//! A JSON catalog at `$TARS_HOME/models.json` recording, per configured
//! provider, the model ids its API last reported. `tars models` (query) reads
//! it so the common case is fast + offline; `tars models update` refreshes it
//! from the live APIs and reports what changed.
//!
//! The library is tars-owned state under the tars home dir (resolved via
//! `tars_config::resolve_home`), alongside `config.toml` and the event store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Current on-disk schema version. Bump on a breaking shape change; the
/// loader tolerates a missing/older version by treating the file as absent
/// (a fresh `update` rewrites it) rather than crashing.
pub const LIBRARY_VERSION: u32 = 1;

/// File name under `$TARS_HOME`.
pub const LIBRARY_FILE: &str = "models.json";

/// Resolve the library path for a given tars home directory.
pub fn library_path(home: &Path) -> PathBuf {
    home.join(LIBRARY_FILE)
}

/// Outcome class recorded for a provider in the library. Typed (not a magic
/// string) so a consumer branches on the variant; the human-readable reason
/// rides in [`ProviderEntry::note`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    /// Live list retrieved and stored in `models`.
    Ok,
    /// Provider needs a key whose env var is unset (`note` = var name).
    NoKey,
    /// Provider has no list API (CLI / bedrock / mock / cassette).
    Skipped,
    /// Server rejected the credential (401/403).
    AuthFailed,
    /// Server answered with another non-2xx (`note` = the status).
    HttpError,
    /// Could not reach the server (`note` = detail).
    Unreachable,
    /// 2xx but the body didn't parse (`note` = detail).
    ParseError,
}

/// One provider's row in the library.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// The provider `type` (`gemini`, `openai_compat`, …), for display.
    #[serde(rename = "type")]
    pub provider_type: String,
    /// The provider's configured `default_model` at update time.
    pub default_model: String,
    pub status: EntryStatus,
    /// Model ids, sorted. Empty unless `status == Ok`.
    #[serde(default)]
    pub models: Vec<String>,
    /// Human-readable detail for a non-`Ok` status (the real reason — a
    /// missing-var name, an HTTP status, a connect error), never a sentinel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// RFC3339 timestamp of when this row was queried.
    pub queried_at: String,
}

impl ProviderEntry {
    /// Whether the configured `default_model` is absent from the live list.
    /// Only meaningful for `Ok` rows; `false` otherwise (we can't tell).
    pub fn default_is_stale(&self) -> bool {
        self.status == EntryStatus::Ok && !self.models.iter().any(|m| m == &self.default_model)
    }
}

/// The whole persisted catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLibrary {
    pub version: u32,
    /// RFC3339 timestamp of the last full/partial update.
    pub updated_at: String,
    /// provider-name → entry (BTreeMap = stable, sorted serialization).
    pub providers: BTreeMap<String, ProviderEntry>,
}

impl ModelLibrary {
    pub fn new(updated_at: String) -> Self {
        Self {
            version: LIBRARY_VERSION,
            updated_at,
            providers: BTreeMap::new(),
        }
    }

    /// Load the library from `path`. `Ok(None)` when the file does not exist
    /// or is an incompatible older version (caller: "run `tars models
    /// update`"). `Err` only on an actual read/parse failure of a
    /// present, current-version file.
    pub fn load(path: &Path) -> std::io::Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        match serde_json::from_slice::<ModelLibrary>(&bytes) {
            Ok(lib) if lib.version == LIBRARY_VERSION => Ok(Some(lib)),
            // Wrong version → treat as absent so a fresh update rewrites it.
            Ok(_) => Ok(None),
            Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        }
    }

    /// Serialize (pretty) and write atomically-ish to `path`, creating the
    /// parent dir if needed.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // Write to a temp sibling then rename, so a crash mid-write can't
        // truncate the existing library.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)
    }
}

/// Set-difference of two model id lists (both assumed sorted+deduped, as
/// [`crate::model_query::parse_models`] returns). Pure — the tested seam.
///
/// Returns `(added, removed)`: ids in `new` but not `old`, and in `old`
/// but not `new`.
pub fn diff_models(old: &[String], new: &[String]) -> (Vec<String>, Vec<String>) {
    let old_set: std::collections::BTreeSet<&str> = old.iter().map(String::as_str).collect();
    let new_set: std::collections::BTreeSet<&str> = new.iter().map(String::as_str).collect();
    let added = new_set
        .difference(&old_set)
        .map(|s| s.to_string())
        .collect();
    let removed = old_set
        .difference(&new_set)
        .map(|s| s.to_string())
        .collect();
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(models: &[&str], default: &str, status: EntryStatus) -> ProviderEntry {
        ProviderEntry {
            provider_type: "gemini".into(),
            default_model: default.into(),
            status,
            models: models.iter().map(|s| s.to_string()).collect(),
            note: None,
            queried_at: "2026-07-07T00:00:00Z".into(),
        }
    }

    #[test]
    fn diff_reports_added_and_removed() {
        let old = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let new = vec!["b".to_string(), "c".to_string(), "d".to_string()];
        let (added, removed) = diff_models(&old, &new);
        assert_eq!(added, vec!["d"]);
        assert_eq!(removed, vec!["a"]);
    }

    #[test]
    fn diff_empty_old_is_all_added() {
        let (added, removed) = diff_models(&[], &["x".to_string(), "y".to_string()]);
        assert_eq!(added, vec!["x", "y"]);
        assert!(removed.is_empty());
    }

    #[test]
    fn stale_default_detected_only_for_ok_and_absent() {
        // Ok + default present → not stale.
        assert!(!entry(&["gemini-2.5-flash"], "gemini-2.5-flash", EntryStatus::Ok).default_is_stale());
        // Ok + default absent → stale (the leftover-preview case).
        assert!(entry(&["gemini-2.5-flash"], "gemini-3-flash-preview", EntryStatus::Ok).default_is_stale());
        // Non-Ok → we can't tell → not flagged stale.
        assert!(!entry(&[], "whatever", EntryStatus::NoKey).default_is_stale());
    }

    #[test]
    fn library_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = library_path(dir.path());
        let mut lib = ModelLibrary::new("2026-07-07T12:00:00Z".into());
        lib.providers.insert(
            "gemini_flash".into(),
            entry(&["gemini-2.5-flash", "gemini-2.5-pro"], "gemini-2.5-flash", EntryStatus::Ok),
        );
        lib.save(&path).unwrap();

        let back = ModelLibrary::load(&path).unwrap().expect("present");
        assert_eq!(back, lib);
    }

    #[test]
    fn load_missing_file_is_none_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let got = ModelLibrary::load(&library_path(dir.path())).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn load_wrong_version_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = library_path(dir.path());
        std::fs::write(&path, r#"{"version":999,"updated_at":"x","providers":{}}"#).unwrap();
        assert!(ModelLibrary::load(&path).unwrap().is_none());
    }
}
