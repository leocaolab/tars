//! Records a real provider's answers and replays them, so a test of a code change does not also
//! see model noise. A cassette is a map from a request's fingerprint ([`request_fingerprint`]) to
//! the whole event sequence (tool calls included) the provider answered it with; replay looks the
//! fingerprint up and serves what is stored there.
//!
//! It does not compare requests. Whether a request was recorded, and how it differs from the
//! recording it should have matched, is the test's question, asked before the call in
//! `tars_eval::cassette_diff`. A request that reaches replay unrecorded is refused as invalid.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{StreamExt, stream};

use crate::ProviderError;
use tars_types::{
    Caller, ChatEvent, ChatRequest, OutputLimit, Pricing, ProviderId, ProviderProfile,
};

use crate::provider::{LlmEventStream, LlmProvider};

/// One recorded call. `request` is the canonical request text ([`canon_request`]) recording
/// wrote down for a test to diff a changed request against; replay does not read it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Recording {
    pub request: String,
    pub events: Vec<ChatEvent>,
}

/// Returns the fingerprint of [`canon_request`]: the lowercase hex SHA-256 of the canonical text,
/// so it stays the same across Rust releases and platforms that a committed cassette is replayed
/// on.
pub fn request_fingerprint(req: &ChatRequest, model: &str) -> String {
    fingerprint_of_canon(&canon_request(req, model))
}

fn fingerprint_of_canon(canon: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(canon.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Returns the canonical text of a request: the model, then the whole serialized request.
pub fn canon_request(req: &ChatRequest, model: &str) -> String {
    // A Debug fallback would make a fingerprint that changes across Rust releases.
    let body = serde_json::to_string(req).expect(
        "ChatRequest serializes: its maps (tool schemas, arguments) are serde_json maps with \
         string keys, it has no custom Serialize that fails, and serde_json writes a non-finite \
         temperature as null",
    );
    format!("model={model}\0{body}")
}

enum CassetteMode {
    /// Writes `flush_path`, when set, after every capture and again on drop.
    Record {
        inner: Arc<dyn LlmProvider>,
        captured: Mutex<HashMap<String, Recording>>,
        /// The inner provider's output limit for each model asked about, stored so a replay
        /// fills `max_output_tokens` as the recording did.
        output_limits: Mutex<BTreeMap<String, OutputLimit>>,
        flush_path: Option<PathBuf>,
    },
    Replay {
        cassette: HashMap<String, Recording>,
        output_limits: BTreeMap<String, OutputLimit>,
    },
}

pub struct CassetteProvider {
    id: ProviderId,
    capabilities: ProviderProfile,
    mode: CassetteMode,
}

/// A cassette file. It stores the recorded provider's capabilities because a caller builds a
/// different request for a provider with tools than for one without, and a replay advertising
/// other capabilities would miss every time. `recordings` is a `BTreeMap` so the file diffs well.
#[derive(serde::Serialize)]
pub struct CassetteFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ProviderProfile>,
    /// The recorded provider's output limit per model, stored because the service fills an
    /// unset `max_output_tokens` from it and that value is fingerprinted. A cassette without it
    /// replays with no limit, as it was recorded.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub output_limits: BTreeMap<String, OutputLimit>,
    /// Every key is the fingerprint of its recording's request: only [`CassetteFile::new`], which
    /// computes the keys, and [`CassetteFile::read`], which refuses any other key, make one.
    recordings: BTreeMap<String, Recording>,
}

/// The file as it is decoded, before its keys are checked against their requests.
#[derive(serde::Deserialize)]
struct CassetteFileWire {
    #[serde(default)]
    capabilities: Option<ProviderProfile>,
    #[serde(default)]
    output_limits: BTreeMap<String, OutputLimit>,
    recordings: BTreeMap<String, Recording>,
}

/// A cassette file that does not read.
#[derive(Debug, thiserror::Error)]
pub enum CassetteFileError {
    #[error("the cassette file does not read")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the file is not a cassette file")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Misfiled(#[from] MisfiledRecordingError),
}

/// A recording a cassette file holds under a key that is not its request's fingerprint, which
/// happens only when the file was edited by hand or damaged.
#[derive(Debug, thiserror::Error)]
#[error(
    "the cassette file is corrupt: recording {key} holds a request whose fingerprint is {fingerprint}"
)]
pub struct MisfiledRecordingError {
    pub key: String,
    pub fingerprint: String,
}

/// Returns the fingerprint of `request` when it is not `key`, the key the request is filed under.
fn misfiled_fingerprint(key: &str, request: &str) -> Option<String> {
    let fingerprint = fingerprint_of_canon(request);
    (fingerprint != key).then_some(fingerprint)
}

impl CassetteFile {
    /// Returns a cassette file holding `recordings`, each under its request's fingerprint.
    pub fn new(
        capabilities: Option<ProviderProfile>,
        output_limits: BTreeMap<String, OutputLimit>,
        recordings: impl IntoIterator<Item = Recording>,
    ) -> Self {
        Self {
            capabilities,
            output_limits,
            recordings: recordings
                .into_iter()
                .map(|r| (fingerprint_of_canon(&r.request), r))
                .collect(),
        }
    }

    /// Returns the recordings by the fingerprint of their requests.
    pub fn recordings(&self) -> &BTreeMap<String, Recording> {
        &self.recordings
    }

    /// Reads a cassette file.
    ///
    /// # Errors
    ///
    /// The file does not read ([`CassetteFileError::Io`]), is not a cassette file
    /// ([`CassetteFileError::Decode`]), or holds a recording under a key that is not its
    /// request's fingerprint ([`CassetteFileError::Misfiled`]).
    pub fn read(path: &std::path::Path) -> Result<Self, CassetteFileError> {
        let raw = std::fs::read_to_string(path).map_err(|source| CassetteFileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let wire = serde_json::from_str(&raw).map_err(|source| CassetteFileError::Decode {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self::from_wire(wire)?)
    }

    /// Returns the file `wire` decodes to, refusing it at the first recording filed under a key
    /// that is not its request's fingerprint.
    fn from_wire(wire: CassetteFileWire) -> Result<Self, MisfiledRecordingError> {
        for (key, recording) in &wire.recordings {
            if let Some(fingerprint) = misfiled_fingerprint(key, &recording.request) {
                return Err(MisfiledRecordingError {
                    key: key.clone(),
                    fingerprint,
                });
            }
        }
        Ok(Self {
            capabilities: wire.capabilities,
            output_limits: wire.output_limits,
            recordings: wire.recordings,
        })
    }

    /// Returns the output limit replay reports for `model`; a model the file has none for
    /// replays with no limit, as it was recorded.
    pub fn output_limit(&self, model: &str) -> OutputLimit {
        output_limit_of(&self.output_limits, model)
    }
}

fn output_limit_of(limits: &BTreeMap<String, OutputLimit>, model: &str) -> OutputLimit {
    limits.get(model).cloned().unwrap_or(OutputLimit::Unknown)
}

impl CassetteProvider {
    pub fn replay(id: ProviderId, cassette: HashMap<String, Recording>) -> Arc<Self> {
        Self::replay_with_caps(id, cassette, None, BTreeMap::new())
    }

    /// Returns a replaying cassette that advertises `caps`, the recorded provider's, so the caller
    /// builds the request it built when recording; `None` advertises the text-only baseline.
    pub fn replay_with_caps(
        id: ProviderId,
        cassette: HashMap<String, Recording>,
        caps: Option<ProviderProfile>,
        output_limits: BTreeMap<String, OutputLimit>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            capabilities: caps.unwrap_or_else(|| {
                // Marked `Mock`: a cassette makes no real call.
                let mut c = ProviderProfile::text_only_baseline(Pricing::default());
                c.interface = tars_types::InterfaceKind::Mock;
                c
            }),
            mode: CassetteMode::Replay {
                cassette,
                output_limits,
            },
        })
    }

    pub fn record(id: ProviderId, inner: Arc<dyn LlmProvider>) -> Arc<Self> {
        Self::record_to(id, inner, None)
    }

    /// Returns a recording cassette over `inner` that writes `flush_path` after each capture.
    pub fn record_to(
        id: ProviderId,
        inner: Arc<dyn LlmProvider>,
        flush_path: Option<PathBuf>,
    ) -> Arc<Self> {
        Self::record_seeded(id, inner, flush_path, HashMap::new())
    }

    /// Returns [`Self::record_to`] starting from the recordings in `seed`, so a session split
    /// across several registry builds adds to the file instead of each build overwriting it.
    pub fn record_seeded(
        id: ProviderId,
        inner: Arc<dyn LlmProvider>,
        flush_path: Option<PathBuf>,
        seed: HashMap<String, Recording>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            capabilities: inner.capabilities().clone(),
            mode: CassetteMode::Record {
                inner,
                captured: Mutex::new(seed),
                output_limits: Mutex::new(BTreeMap::new()),
                flush_path,
            },
        })
    }

    /// Loads a cassette file for replay.
    ///
    /// # Errors
    ///
    /// As [`CassetteFile::read`].
    pub fn replay_from_file(
        id: ProviderId,
        path: &std::path::Path,
    ) -> Result<Arc<Self>, CassetteFileError> {
        let file = CassetteFile::read(path)?;
        Ok(Self::replay_with_caps(
            id,
            file.recordings.into_iter().collect(),
            file.capabilities,
            file.output_limits,
        ))
    }

    /// Returns the recordings of a cassette file, for a recording session to start from.
    ///
    /// # Errors
    ///
    /// As [`CassetteFile::read`].
    pub fn recordings_from_file(
        path: &std::path::Path,
    ) -> Result<HashMap<String, Recording>, CassetteFileError> {
        Ok(CassetteFile::read(path)?.recordings.into_iter().collect())
    }

    pub fn take_captured(&self) -> HashMap<String, Recording> {
        match &self.mode {
            CassetteMode::Record { captured, .. } => {
                std::mem::take(&mut *captured.lock().unwrap_or_else(|e| e.into_inner()))
            }
            CassetteMode::Replay { .. } => HashMap::new(),
        }
    }
}

/// Why a recording session's cassette file was not written.
#[derive(Debug, thiserror::Error)]
pub enum WriteCassetteError {
    #[error("cannot create the cassette directory {}", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write the cassette file {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot serialize the cassette")]
    Encode(#[source] serde_json::Error),
}

/// Writes the cassette file; an empty `map` writes nothing.
fn write_cassette(
    map: &HashMap<String, Recording>,
    caps: &ProviderProfile,
    output_limits: &Mutex<BTreeMap<String, OutputLimit>>,
    path: &std::path::Path,
) -> Result<(), WriteCassetteError> {
    if map.is_empty() {
        return Ok(());
    }
    let file = CassetteFile::new(
        Some(caps.clone()),
        output_limits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        map.values().cloned(),
    );
    let json = serde_json::to_string_pretty(&file).map_err(WriteCassetteError::Encode)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| WriteCassetteError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, json).map_err(|source| WriteCassetteError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    tracing::debug!(path = %path.display(), entries = file.recordings.len(), "cassette flushed");
    Ok(())
}

/// Writes the cassette file when the provider is dropped and logs a failure, since a destructor
/// has no caller to tell.
fn flush_cassette(
    map: &HashMap<String, Recording>,
    caps: &ProviderProfile,
    output_limits: &Mutex<BTreeMap<String, OutputLimit>>,
    path: &std::path::Path,
) {
    if let Err(e) = write_cassette(map, caps, output_limits, path) {
        tracing::warn!(error = %tars_utils::display_chain(&e), "cassette flush failed");
    }
}

#[async_trait]
impl LlmProvider for CassetteProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn capabilities(&self) -> &ProviderProfile {
        &self.capabilities
    }

    fn output_limit(&self, model: &str) -> OutputLimit {
        match &self.mode {
            CassetteMode::Record {
                inner,
                output_limits,
                ..
            } => {
                let limit = inner.output_limit(model);
                output_limits
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(model.to_string(), limit.clone());
                limit
            }
            CassetteMode::Replay { output_limits, .. } => output_limit_of(output_limits, model),
        }
    }

    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: Caller,
    ) -> Result<LlmEventStream, ProviderError> {
        let canon = canon_request(&req, model);
        let key = fingerprint_of_canon(&canon);
        match &self.mode {
            CassetteMode::Replay { cassette, .. } => match cassette.get(&key) {
                Some(rec) => {
                    let out: Vec<Result<ChatEvent, ProviderError>> =
                        rec.events.iter().cloned().map(Ok).collect();
                    Ok(Box::pin(stream::iter(out)))
                }
                None => Err(ProviderError::InvalidRequest(
                    crate::provider::error::RequestRefusal::NoRecording {
                        cassette: self.id.clone(),
                        fingerprint: key,
                        model: model.to_string(),
                    },
                )),
            },
            CassetteMode::Record {
                inner,
                captured,
                output_limits,
                flush_path,
            } => {
                // Collects the whole stream before passing it on, and stores it only when no event
                // failed, so a failed call is never frozen as a response.
                let events: Vec<Result<ChatEvent, ProviderError>> =
                    inner.clone().stream(req, model, ctx).await?.collect().await;
                if events.iter().all(|e| e.is_ok()) {
                    let recording = Recording {
                        request: canon,
                        events: events.iter().map(|e| e.as_ref().unwrap().clone()).collect(),
                    };
                    let snapshot = {
                        let mut guard = captured.lock().unwrap_or_else(|e| e.into_inner());
                        guard.insert(key, recording);
                        guard.clone()
                    };
                    // Written now, not only on drop: a host that leaves through
                    // `std::process::exit` runs no destructors. A recording that is lost fails the
                    // call, so whoever records sees it.
                    if let Some(path) = flush_path {
                        write_cassette(&snapshot, &self.capabilities, output_limits, path)
                            .map_err(ProviderError::CassetteNotWritten)?;
                    }
                }
                Ok(Box::pin(stream::iter(events)))
            }
        }
    }
}

impl Drop for CassetteProvider {
    fn drop(&mut self) {
        if let CassetteMode::Record {
            captured,
            output_limits,
            flush_path: Some(path),
            ..
        } = &self.mode
        {
            let map = captured.lock().unwrap_or_else(|e| e.into_inner());
            flush_cassette(&map, &self.capabilities, output_limits, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::backends::mock::{CannedResponse, MockProvider};

    fn req(prompt: &str) -> ChatRequest {
        ChatRequest::user(prompt)
    }

    async fn collect_text(p: Arc<dyn LlmProvider>, r: ChatRequest) -> String {
        p.stream(r, "test-model", Caller::test_default())
            .await
            .unwrap()
            .filter_map(|e| async move {
                match e {
                    Ok(ChatEvent::Delta { text }) => Some(text),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .await
            .join("")
    }

    async fn collect_tool_names(p: Arc<dyn LlmProvider>, r: ChatRequest) -> Vec<String> {
        p.stream(r, "test-model", Caller::test_default())
            .await
            .unwrap()
            .filter_map(|e| async move {
                match e {
                    Ok(ChatEvent::ToolCallStart { name, .. }) => Some(name),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn record_then_replay_round_trips_by_fingerprint() {
        let real = MockProvider::with_responses(
            ProviderId::new("real"),
            vec![
                CannedResponse::text("FINDING_A"),
                CannedResponse::text("FINDING_B"),
            ],
        );
        let rec = CassetteProvider::record(ProviderId::new("cass"), real);
        assert_eq!(collect_text(rec.clone(), req("file-1")).await, "FINDING_A");
        assert_eq!(collect_text(rec.clone(), req("file-2")).await, "FINDING_B");
        let cassette = rec.take_captured();
        assert_eq!(cassette.len(), 2);

        let play = CassetteProvider::replay(ProviderId::new("cass"), cassette);
        assert_eq!(collect_text(play.clone(), req("file-1")).await, "FINDING_A");
        assert_eq!(collect_text(play.clone(), req("file-2")).await, "FINDING_B");
        assert_eq!(collect_text(play.clone(), req("file-1")).await, "FINDING_A");
    }

    #[tokio::test]
    async fn replay_preserves_tool_calls_not_just_text() {
        // A tool call, so a tool loop can be frozen too.
        use tars_types::{StopReason, Usage};
        let tool_resp = CannedResponse::Sequence(vec![
            ChatEvent::started("real"),
            ChatEvent::ToolCallStart {
                index: 0,
                id: "c1".into(),
                name: "fs.write_file".into(),
            },
            ChatEvent::ToolCallEnd {
                index: 0,
                id: "c1".into(),
                parsed_args: serde_json::json!({"path": "a.rs", "content": "fixed"})
                    .as_object()
                    .unwrap()
                    .clone(),
                thought_signature: None,
            },
            ChatEvent::Finished {
                stop_reason: StopReason::ToolUse,
                usage: Some(Usage::default()),
            },
        ]);
        let real = MockProvider::with_responses(ProviderId::new("real"), vec![tool_resp]);
        let rec = CassetteProvider::record(ProviderId::new("cass"), real);
        assert_eq!(
            collect_tool_names(rec.clone(), req("fix")).await,
            vec!["fs.write_file"]
        );
        let cassette = rec.take_captured();

        let play = CassetteProvider::replay(ProviderId::new("cass"), cassette);
        assert_eq!(
            collect_tool_names(play.clone(), req("fix")).await,
            vec!["fs.write_file"]
        );
    }

    #[test]
    fn cassette_file_round_trips_capabilities() {
        let mut caps = ProviderProfile::text_only_baseline(Pricing::default());
        caps.tool_use = tars_types::ToolUse::Supported { parallel: false };
        let limit = OutputLimit::Configured { tokens: 4242 };
        let file = CassetteFile {
            capabilities: Some(caps.clone()),
            output_limits: BTreeMap::from([("m".to_string(), limit.clone())]),
            recordings: BTreeMap::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let json = serde_json::to_string(&file).unwrap();
        let back = CassetteFile::read(&written_cassette(&dir, &json)).unwrap();
        assert_eq!(back.output_limits.get("m"), Some(&limit));
        assert_eq!(
            back.capabilities.unwrap().tool_use.support(),
            tars_types::Support::Supported,
            "caps survive the cassette file"
        );
        let replay = CassetteProvider::replay_with_caps(
            ProviderId::new("c"),
            HashMap::new(),
            None,
            back.output_limits,
        );
        assert_eq!(replay.output_limit("m"), limit);
        assert_eq!(replay.output_limit("other"), OutputLimit::Unknown);
        let legacy = CassetteFile::read(&written_cassette(&dir, r#"{"recordings":{}}"#))
            .expect("pre-limits cassette loads");
        assert!(legacy.output_limits.is_empty());
        // A bare map has no `recordings` key and does not load.
        assert!(matches!(
            CassetteFile::read(&written_cassette(&dir, r#"{"abc":[]}"#)),
            Err(CassetteFileError::Decode { .. })
        ));
    }

    fn written_cassette(dir: &tempfile::TempDir, json: &str) -> PathBuf {
        let path = dir.path().join("cassette.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    fn cassette_json() -> (String, String) {
        let recording = Recording {
            request: canon_request(&req("review lib.rs"), "m"),
            events: vec![],
        };
        let file = CassetteFile::new(None, BTreeMap::new(), [recording]);
        let key = file.recordings().keys().next().unwrap().clone();
        (serde_json::to_string(&file).unwrap(), key)
    }

    #[test]
    fn a_file_whose_keys_are_their_requests_fingerprints_reads() {
        let dir = tempfile::tempdir().unwrap();
        let (json, key) = cassette_json();
        let file = CassetteFile::read(&written_cassette(&dir, &json)).unwrap();
        assert!(file.recordings().contains_key(&key));
    }

    #[test]
    fn a_file_with_a_key_changed_by_one_character_is_refused_as_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let (json, key) = cassette_json();
        let last = if key.ends_with('0') { "1" } else { "0" };
        let edited_key = format!("{}{last}", &key[..key.len() - 1]);
        let edited = json.replace(&key, &edited_key);

        let err = CassetteFile::read(&written_cassette(&dir, &edited))
            .err()
            .expect("a misfiled recording is refused");
        let message = err.to_string();
        assert!(message.contains("corrupt"), "{message}");
        assert!(
            message.contains(&edited_key),
            "names the stored key: {message}"
        );
        assert!(
            message.contains(&key),
            "names the request's fingerprint: {message}"
        );
    }
    fn one_character_off(key: &str) -> String {
        let last = if key.ends_with('0') { "1" } else { "0" };
        format!("{}{last}", &key[..key.len() - 1])
    }

    #[test]
    fn a_request_filed_under_its_own_fingerprint_is_not_misfiled() {
        let request = canon_request(&req("review lib.rs"), "m");
        let key = fingerprint_of_canon(&request);
        assert_eq!(misfiled_fingerprint(&key, &request), None);
    }

    #[test]
    fn a_request_filed_under_a_key_one_character_off_is_misfiled_with_its_fingerprint() {
        let request = canon_request(&req("review lib.rs"), "m");
        let key = fingerprint_of_canon(&request);
        assert_eq!(
            misfiled_fingerprint(&one_character_off(&key), &request),
            Some(key)
        );
    }

    #[test]
    fn reading_a_misfiled_file_returns_the_stored_key_and_the_requests_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let (json, key) = cassette_json();
        let edited_key = one_character_off(&key);
        let edited = json.replace(&key, &edited_key);

        match CassetteFile::read(&written_cassette(&dir, &edited)) {
            Err(CassetteFileError::Misfiled(MisfiledRecordingError {
                key: stored,
                fingerprint,
            })) => {
                assert_eq!(stored, edited_key);
                assert_eq!(fingerprint, key);
            }
            other => panic!("expected Misfiled, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn reading_a_missing_file_is_an_io_error_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.json");
        match CassetteFile::read(&path) {
            Err(CassetteFileError::Io { path: named, .. }) => assert_eq!(named, path),
            other => panic!("expected Io, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn reading_a_file_that_is_not_a_cassette_is_a_decode_error_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = written_cassette(&dir, "not a cassette");
        match CassetteFile::read(&path) {
            Err(CassetteFileError::Decode { path: named, .. }) => assert_eq!(named, path),
            other => panic!("expected Decode, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn a_cassette_directory_that_cannot_be_created_is_the_reported_cause() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let path = blocker.join("sub").join("c.json");
        let map = HashMap::from([(
            "k".to_string(),
            Recording {
                request: String::new(),
                events: Vec::new(),
            },
        )]);
        let caps = ProviderProfile::text_only_baseline(Pricing::default());

        let err = write_cassette(&map, &caps, &Mutex::new(BTreeMap::new()), &path).unwrap_err();

        assert!(
            matches!(&err, WriteCassetteError::CreateDir { path, .. } if *path == blocker.join("sub")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_recording_that_cannot_be_written_fails_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let inner = MockProvider::new(ProviderId::new("inner"), CannedResponse::text("hi"));
        let cassette = CassetteProvider::record_to(
            ProviderId::new("cass"),
            inner,
            Some(blocker.join("c.json")),
        );

        let sent = cassette
            .stream(req("q"), "test-model", Caller::test_default())
            .await;

        assert!(
            matches!(sent, Err(ProviderError::CassetteNotWritten(_))),
            "{:?}",
            sent.err()
        );
    }
}
