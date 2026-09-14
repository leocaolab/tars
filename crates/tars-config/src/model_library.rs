//! The persisted **model library** — the LIVE layer of the model catalog.
//!
//! A JSON file at `$TARS_HOME/models.json` recording, per configured provider,
//! the models its list API last reported: each id, plus the context and output
//! limits when the API states them (Gemini does; OpenAI-shaped lists carry ids
//! only). `tars models update` writes it; `tars models` reads it for the fast
//! offline listing; and the runtime reads it through
//! [`crate::model_catalog::ModelCatalog`], where a live limit wins over the
//! shipped `provider.toml` number and `<series>@latest` picks from the live
//! ids — so a refresh takes effect without a tars release.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Current on-disk schema version. Bump on a breaking shape change; the
/// loader treats a different version as absent (the next `update` rewrites
/// it) rather than crashing.
pub const LIBRARY_VERSION: u32 = 2;

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

/// One model as the provider's list API reported it. A limit is `None` when
/// the API does not state it — never a guessed number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveModel {
    pub id: String,
    /// Input-token limit (context window), when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// Output-token limit, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
}

impl LiveModel {
    /// A model the API listed by id only.
    pub fn id_only(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            context: None,
            max_output: None,
        }
    }
}

/// One provider's row in the library.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// The provider `type` (`gemini`, `openai_compat`, …), for display.
    #[serde(rename = "type")]
    pub provider_type: String,
    /// The `data/provider.toml` block this provider's models belong to
    /// (`gemini` for a `gemini_flash` instance; `deepseek` for the
    /// `deepseek` openai_compat instance) — see
    /// [`crate::ProviderConfig::catalog_name`]. The runtime overlays this
    /// row onto that block.
    pub catalog: String,
    /// The provider's configured `default_model` spec at update time.
    pub default_model: String,
    pub status: EntryStatus,
    /// Models, sorted by id. Empty unless `status == Ok`.
    #[serde(default)]
    pub models: Vec<LiveModel>,
    /// Human-readable detail for a non-`Ok` status (the real reason — a
    /// missing-var name, an HTTP status, a connect error), never a sentinel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// RFC3339 timestamp of when this row was queried.
    pub queried_at: String,
}

impl ProviderEntry {
    /// The listed ids, in order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.models.iter().map(|m| m.id.as_str())
    }

    /// The listed row for `id`.
    pub fn model(&self, id: &str) -> Option<&LiveModel> {
        self.models.iter().find(|m| m.id == id)
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
    /// or is another schema version (caller: "run `tars models update`").
    /// `Err` only on an actual read/parse failure of a present,
    /// current-version file.
    pub fn load(path: &Path) -> std::io::Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        // Read the version first so an older shape is "absent", not a parse
        // error.
        #[derive(Deserialize)]
        struct Version {
            version: u32,
        }
        if let Ok(v) = serde_json::from_slice::<Version>(&bytes)
            && v.version != LIBRARY_VERSION
        {
            return Ok(None);
        }
        serde_json::from_slice::<ModelLibrary>(&bytes)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
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

    /// The freshest `Ok` row for each catalog block — several configured
    /// providers can list the same block (`gemini_flash` and `gemini_pro`
    /// both list `gemini`).
    pub fn freshest_by_catalog(&self) -> BTreeMap<&str, &ProviderEntry> {
        let mut out: BTreeMap<&str, &ProviderEntry> = BTreeMap::new();
        for entry in self.providers.values() {
            if entry.status != EntryStatus::Ok {
                continue;
            }
            let slot = out.entry(entry.catalog.as_str()).or_insert(entry);
            // RFC3339 UTC timestamps order lexically.
            if entry.queried_at > slot.queried_at {
                *slot = entry;
            }
        }
        out
    }
}

/// Set-difference of two model id lists. Pure — the tested seam.
///
/// Returns `(added, removed)`: ids in `new` but not `old`, and in `old`
/// but not `new`, each sorted.
pub fn diff_models<'a>(
    old: impl IntoIterator<Item = &'a str>,
    new: impl IntoIterator<Item = &'a str>,
) -> (Vec<String>, Vec<String>) {
    let old_set: std::collections::BTreeSet<&str> = old.into_iter().collect();
    let new_set: std::collections::BTreeSet<&str> = new.into_iter().collect();
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
pub(crate) mod tests {
    use super::*;

    pub(crate) fn entry(catalog: &str, models: &[&str], queried_at: &str) -> ProviderEntry {
        ProviderEntry {
            provider_type: "gemini".into(),
            catalog: catalog.into(),
            default_model: "flash@latest".into(),
            status: EntryStatus::Ok,
            models: models.iter().map(|s| LiveModel::id_only(*s)).collect(),
            note: None,
            queried_at: queried_at.into(),
        }
    }

    #[test]
    fn diff_reports_added_and_removed() {
        let (added, removed) = diff_models(["a", "b", "c"], ["b", "c", "d"]);
        assert_eq!(added, vec!["d"]);
        assert_eq!(removed, vec!["a"]);
    }

    #[test]
    fn diff_empty_old_is_all_added() {
        let (added, removed) = diff_models([], ["x", "y"]);
        assert_eq!(added, vec!["x", "y"]);
        assert!(removed.is_empty());
    }

    #[test]
    fn library_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = library_path(dir.path());
        let mut lib = ModelLibrary::new("2026-09-14T12:00:00Z".into());
        let mut e = entry("gemini", &["gemini-2.5-pro"], "2026-09-14T12:00:00Z");
        e.models.push(LiveModel {
            id: "gemini-3.8-flash".into(),
            context: Some(1_048_576),
            max_output: Some(65_536),
        });
        lib.providers.insert("gemini_flash".into(), e);
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
    fn load_other_version_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = library_path(dir.path());
        // A v1 file (ids as bare strings, no catalog) reads as absent.
        std::fs::write(
            &path,
            r#"{"version":1,"updated_at":"x","providers":{"g":{"type":"gemini","default_model":"m","status":"ok","models":["a"],"queried_at":"x"}}}"#,
        )
        .unwrap();
        assert!(ModelLibrary::load(&path).unwrap().is_none());
    }

    #[test]
    fn freshest_row_per_catalog_wins() {
        let mut lib = ModelLibrary::new("t".into());
        lib.providers.insert(
            "gemini_flash".into(),
            entry("gemini", &["old"], "2026-09-01T00:00:00Z"),
        );
        lib.providers.insert(
            "gemini_pro".into(),
            entry("gemini", &["new"], "2026-09-14T00:00:00Z"),
        );
        let mut failed = entry("deepseek", &[], "2026-09-15T00:00:00Z");
        failed.status = EntryStatus::NoKey;
        lib.providers.insert("deepseek".into(), failed);

        let by = lib.freshest_by_catalog();
        assert_eq!(by["gemini"].models[0].id, "new");
        assert!(!by.contains_key("deepseek"), "a failed query lists nothing");
    }
}
