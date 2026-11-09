//! Reads and writes `<home>/models.json`: for each configured provider, how its last list query
//! ended and the last list its API returned, with the context and output limits when the API
//! states them (Gemini does; OpenAI-shaped lists carry ids only). A query that was not sent or
//! failed does not replace the last list, so the runtime keeps it until a query succeeds again.
//!
//! `tars models update` writes it and `tars models` lists it offline. Through
//! [`crate::model_catalog::ModelCatalog`] a live limit wins over the shipped `provider.toml`
//! number and `<series>@latest` picks from the live ids, so a refresh takes effect without a tars
//! release.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The on-disk schema version, bumped on a breaking shape change. A file of another version
/// loads as absent, and the next `update` rewrites it.
pub const LIBRARY_VERSION: u32 = 3;

pub const LIBRARY_FILE: &str = "models.json";

/// Returns the library path under the tars home `home`.
pub fn library_path(home: &Path) -> PathBuf {
    home.join(LIBRARY_FILE)
}

/// Says why tars did not ask a provider for its list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotAsked {
    /// Means the key's environment variable is unset or blank.
    NoKey,
    /// Means the provider has no list API (CLI, mock, cassette).
    NoListApi,
    /// Means a server the operator runs (vllm, mlx, llama.cpp) refused the connection: it is
    /// not running.
    ServerNotRunning,
}

/// Says why a list query that was sent returned no list; [`Failure::reason`] carries the detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// Means the config names a key that did not resolve: a key file that cannot be read, a
    /// blank inline key, a variable that is not UTF-8.
    KeyUnresolved,
    /// Means the key cannot be sent as a request header (it holds a newline or another byte a
    /// header value refuses).
    KeyInvalid,
    /// Means the server answered 401 or 403.
    AuthFailed,
    /// Means the server answered another non-2xx status.
    HttpError,
    /// Means the server could not be reached.
    Unreachable,
    /// Means a 2xx body did not parse.
    ParseError,
}

/// One model as the provider's list API reported it; a limit the API does not state is `None`,
/// not a guess.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveModel {
    pub id: String,
    /// Is the input-token limit, the context window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// Is the output-token limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
}

impl LiveModel {
    /// Returns a model the API listed by id only.
    pub fn id_only(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            context: None,
            max_output: None,
        }
    }
}

/// The model list one query of a provider's list API returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    /// Is sorted by id.
    #[serde(default)]
    pub models: Vec<LiveModel>,
    /// Is the RFC 3339 UTC time of the query that returned the list.
    pub queried_at: String,
}

impl Listing {
    /// Returns the listed ids in order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.models.iter().map(|m| m.id.as_str())
    }

    /// Returns the listed model `id`.
    pub fn model(&self, id: &str) -> Option<&LiveModel> {
        self.models.iter().find(|m| m.id == id)
    }
}

/// A list query that returned no list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    /// Holds the real reason: the missing variable, the HTTP status, the connect error.
    pub reason: String,
    /// Is an RFC 3339 UTC timestamp.
    pub queried_at: String,
}

/// Says how a provider's last list query ended, and which list the runtime reads for it.
/// `kept` is the list an earlier query returned; it stays the provider's live list until a
/// query succeeds again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "QueryRow", into = "QueryRow")]
pub enum LastQuery {
    Listed(Listing),
    /// Nothing was sent; `note` is the real reason (`no key: set $GEMINI_API_KEY`).
    NotAsked {
        why: NotAsked,
        note: String,
        kept: Option<Listing>,
    },
    Failed {
        failure: Failure,
        kept: Option<Listing>,
    },
}

impl LastQuery {
    /// Returns the provider's live list: the one this query returned, or the one kept from before
    /// it.
    pub fn listing(&self) -> Option<&Listing> {
        match self {
            Self::Listed(listing) => Some(listing),
            Self::NotAsked { kept, .. } | Self::Failed { kept, .. } => kept.as_ref(),
        }
    }

    /// Returns when the query ran, as an RFC 3339 UTC timestamp, or `None` when none was sent.
    pub fn queried_at(&self) -> Option<&str> {
        match self {
            Self::Listed(listing) => Some(&listing.queried_at),
            Self::NotAsked { .. } => None,
            Self::Failed { failure, .. } => Some(&failure.queried_at),
        }
    }

    /// Returns this query with `kept` as the list kept from before it; a listed query keeps
    /// nothing.
    fn keeping(self, kept: Option<Listing>) -> Self {
        match self {
            Self::NotAsked { why, note, .. } => Self::NotAsked { why, note, kept },
            Self::Failed { failure, .. } => Self::Failed { failure, kept },
            listed @ Self::Listed(_) => listed,
        }
    }
}

/// The on-disk shape of a [`LastQuery`]: `status` names how it ended, `note` holds the reason.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum QueryRow {
    Ok(Listing),
    NoKey(NotAskedRow),
    NoListApi(NotAskedRow),
    ServerNotRunning(NotAskedRow),
    KeyUnresolved(FailedRow),
    KeyInvalid(FailedRow),
    AuthFailed(FailedRow),
    HttpError(FailedRow),
    Unreachable(FailedRow),
    ParseError(FailedRow),
}

#[derive(Clone, Serialize, Deserialize)]
struct NotAskedRow {
    note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kept: Option<Listing>,
}

#[derive(Clone, Serialize, Deserialize)]
struct FailedRow {
    note: String,
    queried_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kept: Option<Listing>,
}

impl From<QueryRow> for LastQuery {
    fn from(row: QueryRow) -> Self {
        let not_asked = |why, row: NotAskedRow| Self::NotAsked {
            why,
            note: row.note,
            kept: row.kept,
        };
        let (kind, row) = match row {
            QueryRow::Ok(listing) => return Self::Listed(listing),
            QueryRow::NoKey(row) => return not_asked(NotAsked::NoKey, row),
            QueryRow::NoListApi(row) => return not_asked(NotAsked::NoListApi, row),
            QueryRow::ServerNotRunning(row) => return not_asked(NotAsked::ServerNotRunning, row),
            QueryRow::KeyUnresolved(row) => (FailureKind::KeyUnresolved, row),
            QueryRow::KeyInvalid(row) => (FailureKind::KeyInvalid, row),
            QueryRow::AuthFailed(row) => (FailureKind::AuthFailed, row),
            QueryRow::HttpError(row) => (FailureKind::HttpError, row),
            QueryRow::Unreachable(row) => (FailureKind::Unreachable, row),
            QueryRow::ParseError(row) => (FailureKind::ParseError, row),
        };
        Self::Failed {
            failure: Failure {
                kind,
                reason: row.note,
                queried_at: row.queried_at,
            },
            kept: row.kept,
        }
    }
}

impl From<LastQuery> for QueryRow {
    fn from(query: LastQuery) -> Self {
        let (failure, kept) = match query {
            LastQuery::Listed(listing) => return Self::Ok(listing),
            LastQuery::NotAsked { why, note, kept } => {
                let row = NotAskedRow { note, kept };
                return match why {
                    NotAsked::NoKey => Self::NoKey(row),
                    NotAsked::NoListApi => Self::NoListApi(row),
                    NotAsked::ServerNotRunning => Self::ServerNotRunning(row),
                };
            }
            LastQuery::Failed { failure, kept } => (failure, kept),
        };
        let row = FailedRow {
            note: failure.reason,
            queried_at: failure.queried_at,
            kept,
        };
        match failure.kind {
            FailureKind::KeyUnresolved => Self::KeyUnresolved(row),
            FailureKind::KeyInvalid => Self::KeyInvalid(row),
            FailureKind::AuthFailed => Self::AuthFailed(row),
            FailureKind::HttpError => Self::HttpError(row),
            FailureKind::Unreachable => Self::Unreachable(row),
            FailureKind::ParseError => Self::ParseError(row),
        }
    }
}

/// One provider's row in the library.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEntry {
    #[serde(rename = "type")]
    pub provider_type: crate::providers::ProviderKind,
    /// Names the `data/provider.toml` block this row's list stands for (`gemini` for a
    /// `gemini_flash` instance); see [`crate::ProviderConfig::listing_catalog`].
    pub catalog: String,
    /// Holds the provider's `default_model` spec as it was configured when this row was written,
    /// not the current config's.
    pub default_model: String,
    #[serde(flatten)]
    pub last_query: LastQuery,
}

impl ProviderEntry {
    /// Returns the row `last_query` leaves for the provider declared as `id` with `cfg`.
    pub fn new(id: &str, cfg: &crate::ProviderConfig, last_query: LastQuery) -> Self {
        Self {
            provider_type: cfg.kind(),
            catalog: cfg.listing_catalog(id).to_string(),
            default_model: cfg.default_model().to_string(),
            last_query,
        }
    }

    /// Returns this row as it stands after `prev`, the provider's previous row. A query that was
    /// not sent or failed keeps the list `prev` held for the same catalog block, so one refresh
    /// that lists nothing does not take the live list away from the runtime; a list of another
    /// block is dropped.
    pub fn following(self, prev: Option<&ProviderEntry>) -> Self {
        let kept = prev
            .filter(|p| p.catalog == self.catalog)
            .and_then(|p| p.last_query.listing())
            .cloned();
        Self {
            last_query: self.last_query.keeping(kept),
            ..self
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLibrary {
    pub version: u32,
    /// Is the RFC 3339 timestamp of the last update, whole or partial.
    pub updated_at: String,
    /// Is keyed by provider name; a `BTreeMap` so the file is written in a stable order.
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

    /// Loads the library from `path`, or returns `Ok(None)` when the file does not exist or is
    /// another schema version.
    ///
    /// # Errors
    ///
    /// Returns an error when a present file cannot be read or does not parse.
    pub fn load(path: &Path) -> std::io::Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        // The version is read first so an older shape is absent, not a parse error.
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

    /// Writes the library to `path` as pretty JSON, creating the parent directory.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // A crash mid-write must not truncate the existing library.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)
    }

    /// Returns the freshest live list for each catalog block, since several configured providers
    /// can list one block (`gemini_flash` and `gemini_pro` both list `gemini`). A list a failed
    /// query kept counts at the time it was listed.
    pub fn freshest_by_catalog(&self) -> BTreeMap<&str, &Listing> {
        let mut out: BTreeMap<&str, &Listing> = BTreeMap::new();
        for entry in self.providers.values() {
            let Some(listing) = entry.last_query.listing() else {
                continue;
            };
            let slot = out.entry(entry.catalog.as_str()).or_insert(listing);
            // RFC3339 UTC timestamps order lexically.
            if listing.queried_at > slot.queried_at {
                *slot = listing;
            }
        }
        out
    }
}

/// Returns `(added, removed)`: the ids in `new` but not `old`, and in `old` but not `new`, each
/// sorted.
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
        listed(
            catalog,
            models.iter().map(|s| LiveModel::id_only(*s)).collect(),
            queried_at,
        )
    }

    pub(crate) fn listed(catalog: &str, models: Vec<LiveModel>, queried_at: &str) -> ProviderEntry {
        row(
            catalog,
            LastQuery::Listed(Listing {
                models,
                queried_at: queried_at.into(),
            }),
        )
    }

    fn failed(catalog: &str, queried_at: &str) -> ProviderEntry {
        row(
            catalog,
            LastQuery::Failed {
                failure: Failure {
                    kind: FailureKind::Unreachable,
                    reason: "tcp connect error: Connection refused (os error 61)".into(),
                    queried_at: queried_at.into(),
                },
                kept: None,
            },
        )
    }

    fn row(catalog: &str, last_query: LastQuery) -> ProviderEntry {
        ProviderEntry {
            provider_type: crate::providers::ProviderKind::Gemini,
            catalog: catalog.into(),
            default_model: "flash@latest".into(),
            last_query,
        }
    }

    fn kept(entry: &ProviderEntry) -> Option<&Listing> {
        match &entry.last_query {
            LastQuery::NotAsked { kept, .. } | LastQuery::Failed { kept, .. } => kept.as_ref(),
            LastQuery::Listed(_) => None,
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
        let e = listed(
            "gemini",
            vec![
                LiveModel::id_only("gemini-2.5-pro"),
                LiveModel {
                    id: "gemini-3.8-flash".into(),
                    context: Some(1_048_576),
                    max_output: Some(65_536),
                },
            ],
            "2026-09-14T12:00:00Z",
        );
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
        lib.providers.insert(
            "deepseek".into(),
            failed("deepseek", "2026-09-15T00:00:00Z"),
        );

        let by = lib.freshest_by_catalog();
        assert_eq!(by["gemini"].models[0].id, "new");
        assert!(!by.contains_key("deepseek"), "a failed query lists nothing");
    }

    #[test]
    fn a_failed_query_keeps_the_previous_list() {
        let prev = entry("gemini", &["gemini-9.0-flash"], "2026-09-01T00:00:00Z");
        let now = failed("gemini", "2026-09-14T00:00:00Z").following(Some(&prev));
        assert_eq!(kept(&now), prev.last_query.listing());
        assert_eq!(now.last_query.queried_at(), Some("2026-09-14T00:00:00Z"));
    }

    #[test]
    fn a_query_that_was_not_sent_keeps_the_previous_list_and_has_no_query_time() {
        let prev = entry("gemini", &["gemini-9.0-flash"], "2026-09-01T00:00:00Z");
        let now = row(
            "gemini",
            LastQuery::NotAsked {
                why: NotAsked::NoKey,
                note: "no key: set $GEMINI_API_KEY".into(),
                kept: None,
            },
        )
        .following(Some(&prev));
        assert_eq!(kept(&now), prev.last_query.listing());
        assert_eq!(now.last_query.queried_at(), None);
    }

    #[test]
    fn a_listed_query_replaces_the_previous_list() {
        let prev = entry("gemini", &["gemini-9.0-flash"], "2026-09-01T00:00:00Z");
        let fresh = entry("gemini", &["gemini-9.1-flash"], "2026-09-14T00:00:00Z");
        assert_eq!(fresh.clone().following(Some(&prev)), fresh);
    }

    #[test]
    fn a_failed_query_carries_a_list_kept_by_an_earlier_failure() {
        let listed = entry("gemini", &["gemini-9.0-flash"], "2026-09-01T00:00:00Z");
        let first = failed("gemini", "2026-09-07T00:00:00Z").following(Some(&listed));
        let second = failed("gemini", "2026-09-14T00:00:00Z").following(Some(&first));
        assert_eq!(kept(&second), listed.last_query.listing());
    }

    #[test]
    fn a_failed_query_keeps_nothing_without_a_list_of_its_catalog() {
        let other_block = entry("openai", &["gpt-9"], "2026-09-01T00:00:00Z");
        let never_listed = failed("gemini", "2026-09-07T00:00:00Z");
        for prev in [None, Some(&other_block), Some(&never_listed)] {
            let now = failed("gemini", "2026-09-14T00:00:00Z").following(prev);
            assert_eq!(kept(&now), None);
        }
    }

    #[test]
    fn a_kept_list_is_the_live_list_of_its_catalog() {
        let prev = entry("gemini", &["gemini-9.0-flash"], "2026-09-01T00:00:00Z");
        let mut lib = ModelLibrary::new("t".into());
        lib.providers.insert(
            "gemini_flash".into(),
            failed("gemini", "2026-09-14T00:00:00Z").following(Some(&prev)),
        );
        let by = lib.freshest_by_catalog();
        assert_eq!(by["gemini"].queried_at, "2026-09-01T00:00:00Z");
        assert_eq!(by["gemini"].models[0].id, "gemini-9.0-flash");
    }

    #[test]
    fn a_kept_list_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = library_path(dir.path());
        let prev = entry("gemini", &["gemini-9.0-flash"], "2026-09-01T00:00:00Z");
        let mut lib = ModelLibrary::new("2026-09-14T00:00:00Z".into());
        lib.providers.insert(
            "gemini_flash".into(),
            failed("gemini", "2026-09-14T00:00:00Z").following(Some(&prev)),
        );
        lib.save(&path).unwrap();

        let back = ModelLibrary::load(&path).unwrap().expect("present");
        assert_eq!(back, lib);
    }

    #[test]
    fn a_row_without_kept_reads_as_keeping_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = library_path(dir.path());
        std::fs::write(
            &path,
            r#"{"version":3,"updated_at":"2026-11-06T19:11:12Z","providers":{
                "g":{"type":"gemini","catalog":"gemini","default_model":"flash@latest","status":"ok",
                     "models":[{"id":"gemini-9.0-flash","max_output":65536}],"queried_at":"2026-11-01T00:00:00Z"},
                "d":{"type":"openai_compat","catalog":"deepseek","default_model":"deepseek-flash",
                     "status":"no_key","note":"no key: set $DEEPSEEK_API_KEY"}}}"#,
        )
        .unwrap();

        let lib = ModelLibrary::load(&path).unwrap().expect("present");
        assert_eq!(
            lib.providers["g"].last_query,
            LastQuery::Listed(Listing {
                models: vec![LiveModel {
                    id: "gemini-9.0-flash".into(),
                    context: None,
                    max_output: Some(65_536),
                }],
                queried_at: "2026-11-01T00:00:00Z".into(),
            })
        );
        assert_eq!(
            lib.providers["d"].last_query,
            LastQuery::NotAsked {
                why: NotAsked::NoKey,
                note: "no key: set $DEEPSEEK_API_KEY".into(),
                kept: None,
            }
        );
    }

    #[test]
    fn every_status_word_round_trips() {
        for (word, kind) in [
            ("auth_failed", FailureKind::AuthFailed),
            ("http_error", FailureKind::HttpError),
            ("unreachable", FailureKind::Unreachable),
            ("parse_error", FailureKind::ParseError),
            ("key_unresolved", FailureKind::KeyUnresolved),
            ("key_invalid", FailureKind::KeyInvalid),
        ] {
            let query = LastQuery::Failed {
                failure: Failure {
                    kind,
                    reason: "r".into(),
                    queried_at: "2026-11-06T00:00:00Z".into(),
                },
                kept: None,
            };
            let row = serde_json::to_value(&query).unwrap();
            assert_eq!(row["status"], word);
            assert_eq!(serde_json::from_value::<LastQuery>(row).unwrap(), query);
        }
        for (word, why) in [
            ("no_key", NotAsked::NoKey),
            ("no_list_api", NotAsked::NoListApi),
            ("server_not_running", NotAsked::ServerNotRunning),
        ] {
            let query = LastQuery::NotAsked {
                why,
                note: "r".into(),
                kept: None,
            };
            let row = serde_json::to_value(&query).unwrap();
            assert_eq!(row["status"], word);
            assert_eq!(serde_json::from_value::<LastQuery>(row).unwrap(), query);
        }
    }
}
