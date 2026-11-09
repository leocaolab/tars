//! Uploads frozen files to the deployment's bucket, under the partition path they have on disk,
//! named `<store-id>.<file>` so the directories sharing a bucket never collide. An upload only
//! ever writes a frozen file's own bytes under that file's own key, and an object appears only
//! whole; nothing here renames or deletes one, so a glob of the bucket's `*.parquet` reads every
//! frozen file once. It does not decide which files are frozen (the manifest does) and never reads
//! the bucket to learn what is valid.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use object_store::path::{InvalidPart, Path as ObjectPath, PathPart};
use object_store::{
    MultipartUpload, ObjectStore, ObjectStoreExt, ObjectStoreScheme, PutMode, PutMultipartOptions,
    PutPayload, PutResult,
};
use thiserror::Error;
use url::Url;
use uuid::Uuid;

use super::super::LogError;
use super::compact::DataFile;
use super::partition::{PartitionDir, io_at};
use super::{IoWork, blocking};

const STORE_ID: &str = "_store_id";

/// The most a file is sent in one request. Each request has the client's own timeout, so a file
/// of any size uploads as long as one part fits in it; S3 wants at least 5 MiB in every part but
/// the last.
const PART_BYTES: NonZeroU64 = NonZeroU64::new(8 << 20).expect("8 MiB is not zero");

/// An event-store directory's name in a bucket many directories share: a UUID kept in
/// `<dir>/_store_id`. Only [`StoreId::read_or_create`] makes one, so two processes opening one
/// directory hold the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoreId(Uuid);

impl StoreId {
    /// Reads `<dir>/_store_id`, first creating it with a fresh id if there is none. The new id is
    /// written whole to a file of its own and hard-linked into place, which fails if the name
    /// exists, so a process that loses a race to create it reads the winner's.
    ///
    /// # Errors
    ///
    /// [`BucketSetupError::StoreIdIo`] when `dir` is missing or the file cannot be read or written, and
    /// [`BucketSetupError::StoreIdText`] when the file holds no UUID.
    pub(crate) async fn read_or_create(dir: PathBuf) -> Result<Self, BucketSetupError> {
        tokio::task::spawn_blocking(move || read_or_create(&dir))
            .await
            .map_err(BucketSetupError::StoreIdTask)?
    }
}

fn read_or_create(dir: &Path) -> Result<StoreId, BucketSetupError> {
    let path = dir.join(STORE_ID);
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => create(dir, &path)?,
        Err(source) => return Err(BucketSetupError::StoreIdIo { path, source }),
        Ok(text) => return parse_store_id(&path, text),
    }
    let text = std::fs::read_to_string(&path).map_err(|source| BucketSetupError::StoreIdIo {
        path: path.clone(),
        source,
    })?;
    parse_store_id(&path, text)
}

/// Writes a fresh id beside `path` and links it in unless another process got there first.
fn create(dir: &Path, path: &Path) -> Result<(), BucketSetupError> {
    let fresh = Uuid::new_v4();
    let tmp = dir.join(format!("{STORE_ID}.{fresh}.tmp"));
    let io_at = |path: &Path| {
        let path = path.to_path_buf();
        move |source| BucketSetupError::StoreIdIo { path, source }
    };
    std::fs::write(&tmp, fresh.to_string()).map_err(io_at(&tmp))?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(io_at(&tmp))?;

    let linked = std::fs::hard_link(&tmp, path);
    std::fs::remove_file(&tmp).map_err(io_at(&tmp))?;
    match linked {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(io_at(path)(e)),
        _ => Ok(()),
    }
}

fn parse_store_id(path: &Path, text: String) -> Result<StoreId, BucketSetupError> {
    match Uuid::parse_str(text.trim()) {
        Ok(id) => Ok(StoreId(id)),
        Err(source) => Err(BucketSetupError::StoreIdText {
            path: path.to_path_buf(),
            found: text,
            source,
        }),
    }
}

/// Where frozen files go: a bucket, the prefix within it, and the id of the store they come from.
#[derive(Debug, Clone)]
pub(crate) struct Bucket {
    store: Arc<dyn ObjectStore>,
    prefix: ObjectPath,
    store_id: StoreId,
}

impl Bucket {
    pub(crate) fn new(store: Arc<dyn ObjectStore>, prefix: ObjectPath, store_id: StoreId) -> Self {
        Self {
            store,
            prefix,
            store_id,
        }
    }

    /// Parses `url` (`s3://bucket/prefix`, `gs://…`, `az://…`, `file:///…`) and builds its client
    /// from the variables of `env` the URL's SDK reads (`AWS_*`, `GOOGLE_*`, `AZURE_*`), which
    /// carry the credentials. The cloud schemes are built in only with the `object-store-cloud`
    /// feature.
    ///
    /// # Errors
    ///
    /// [`BucketSetupError::UrlUnparsed`] for text that is no URL, [`BucketSetupError::UrlCredentials`] for a URL
    /// carrying a user or password, and [`BucketSetupError::Client`] for a scheme this build does not
    /// know or a client the environment cannot configure.
    pub(crate) fn from_url(
        url: &str,
        store_id: StoreId,
        env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self, BucketSetupError> {
        let url = Url::parse(url).map_err(BucketSetupError::UrlUnparsed)?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(BucketSetupError::UrlCredentials {
                url: redacted(&url),
            });
        }
        let (scheme, _) =
            ObjectStoreScheme::parse(&url).map_err(|e| BucketSetupError::Client(e.into()))?;
        let config = sdk_variables(sdk_prefix(&scheme), env);
        let (store, prefix) =
            object_store::parse_url_opts(&url, config).map_err(BucketSetupError::Client)?;
        Ok(Self::new(Arc::from(store), prefix, store_id))
    }
}

/// Returns the prefix of the environment variables the SDK of `scheme` reads, or `None` for a
/// store no variable configures. A client given every variable would take generic names such as
/// `ENDPOINT` or `TOKEN`, set for some other program, as its own.
fn sdk_prefix(scheme: &ObjectStoreScheme) -> Option<&'static str> {
    match scheme {
        ObjectStoreScheme::AmazonS3 => Some("AWS_"),
        ObjectStoreScheme::GoogleCloudStorage => Some("GOOGLE_"),
        ObjectStoreScheme::MicrosoftAzure => Some("AZURE_"),
        _ => None,
    }
}

/// Returns the variables of `env` named under `prefix`, none without one.
fn sdk_variables(
    prefix: Option<&str>,
    env: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(String, String)> {
    let Some(prefix) = prefix else {
        return Vec::new();
    };
    env.into_iter()
        // [arc:intentional-handle] reason: a variable that is not UTF-8 is no SDK setting, which
        // are all text; the SDK's own `from_env` skips it the same way.
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| key.starts_with(prefix))
        .collect()
}

/// Returns `url` from its host on, without the user and password before it.
fn redacted(url: &Url) -> String {
    format!("{}://{}", url.scheme(), &url[url::Position::BeforeHost..])
}

/// Why a store could not be set up to ship to a bucket.
#[derive(Debug, Error)]
pub enum BucketSetupError {
    /// The URL text is left out: it may hold a credential this check never got to see.
    #[error("the bucket URL does not parse")]
    UrlUnparsed(#[source] url::ParseError),
    #[error("the bucket URL {url} carries credentials; give them in the environment instead")]
    UrlCredentials { url: String },
    #[error("no client for the bucket URL")]
    Client(#[source] object_store::Error),
    #[error("{}: the store id cannot be read or written", path.display())]
    StoreIdIo { path: PathBuf, source: io::Error },
    #[error("{}: holds {found:?}, not a store id", path.display())]
    StoreIdText {
        path: PathBuf,
        found: String,
        source: uuid::Error,
    },
    #[error("the task reading the store id failed")]
    StoreIdTask(#[source] tokio::task::JoinError),
}

/// Uploads the frozen files of the partition `dir` the bucket has not confirmed, marking each
/// shipped once the bucket holds it. It stops at the first failure; files marked before it stay
/// marked, and the rest are tried again the next time.
pub(super) async fn upload_partition(
    root: &Path,
    part: PartitionDir,
    bucket: &Bucket,
) -> Result<(), LogError> {
    let pending = {
        let part = part.clone();
        let work = IoWork::Upload(part.path().to_path_buf());
        blocking(work, move || part.pending_uploads()).await?
    };
    for file in pending {
        upload_file(root, &part, file, bucket).await?;
    }
    Ok(())
}

async fn upload_file(
    root: &Path,
    part: &PartitionDir,
    file: DataFile,
    bucket: &Bucket,
) -> Result<(), LogError> {
    let path = part.file_path(&file);
    let key = path
        .strip_prefix(root)
        .map_err(|_| ObjectKeyError::OutsideRoot)
        .and_then(|rel| object_key(&bucket.prefix, bucket.store_id, rel))
        .map_err(|source| LogError::ObjectKey {
            path: path.clone(),
            source,
        })?;

    send_file(bucket.store.as_ref(), &key, &path, PART_BYTES).await?;

    let part = part.clone();
    blocking(IoWork::Upload(path), move || part.mark_uploaded(&file)).await
}

/// How a file of some length goes to the bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transfer {
    /// One create-only put, which the bucket refuses if the object is there.
    OnePut,
    /// A multipart upload, which overwrites on completion; only a file whose object is not there
    /// yet is sent this way.
    Parts,
}

fn transfer(len: u64, part: NonZeroU64) -> Transfer {
    if len <= part.get() {
        Transfer::OnePut
    } else {
        Transfer::Parts
    }
}

/// Puts the file at `path` under `key` unless an object is already there, in parts of `part`
/// bytes when it is longer than one.
async fn send_file(
    store: &dyn ObjectStore,
    key: &ObjectPath,
    path: &Path,
    part: NonZeroU64,
) -> Result<(), LogError> {
    let upload_error = |source| LogError::Upload {
        path: path.to_path_buf(),
        source: Box::new(source),
    };
    let len = {
        let path = path.to_path_buf();
        blocking(IoWork::Upload(path.clone()), move || {
            std::fs::metadata(&path)
                .map(|m| m.len())
                .map_err(io_at(&path))
        })
        .await?
    };

    match transfer(len, part) {
        Transfer::OnePut => {
            let bytes = {
                let path = path.to_path_buf();
                blocking(IoWork::Upload(path.clone()), move || {
                    std::fs::read(&path).map_err(io_at(&path))
                })
                .await?
            };
            let put = store
                .put_opts(key, PutPayload::from(bytes), PutMode::Create.into())
                .await;
            landed(put).map_err(upload_error)
        }
        Transfer::Parts => match store.head(key).await {
            Ok(_) => Ok(()),
            Err(object_store::Error::NotFound { .. }) => {
                let mut upload = store
                    .put_multipart_opts(key, PutMultipartOptions::default())
                    .await
                    .map_err(upload_error)?;
                let sent = send_parts(upload.as_mut(), path, len, part).await;
                if sent.is_err()
                    && let Err(abort) = upload.abort().await
                {
                    tracing::warn!(
                        object = %key,
                        error = %tars_utils::display_chain(&abort),
                        "aborting a failed multipart upload failed; its parts stay in the bucket"
                    );
                }
                sent
            }
            Err(e) => Err(upload_error(e)),
        },
    }
}

/// Sends `len` bytes of the file at `path` as parts of `part` bytes and completes the upload.
async fn send_parts(
    upload: &mut dyn MultipartUpload,
    path: &Path,
    len: u64,
    part: NonZeroU64,
) -> Result<(), LogError> {
    let upload_error = |source| LogError::Upload {
        path: path.to_path_buf(),
        source: Box::new(source),
    };
    let mut file = {
        let path = path.to_path_buf();
        blocking(IoWork::Upload(path.clone()), move || {
            File::open(&path).map_err(io_at(&path))
        })
        .await?
    };
    let mut sent = 0;
    while sent < len {
        let n = part.get().min(len - sent);
        let (back, bytes) = {
            let path = path.to_path_buf();
            blocking(IoWork::Upload(path.clone()), move || {
                read_part(&mut file, n)
                    .map(|bytes| (file, bytes))
                    .map_err(io_at(&path))
            })
            .await?
        };
        file = back;
        upload
            .put_part(PutPayload::from(bytes))
            .await
            .map_err(upload_error)?;
        sent += n;
    }
    upload.complete().await.map(drop).map_err(upload_error)
}

/// Reads the next `n` bytes of `file`; a file shorter than its length said fails here.
fn read_part(file: &mut File, n: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(n).read_to_end(&mut bytes)?;
    if (bytes.len() as u64) < n {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(bytes)
}

/// Why a frozen file has no object name in the bucket.
#[derive(Debug, Error)]
pub enum ObjectKeyError {
    #[error("the file is not under the store's root")]
    OutsideRoot,
    #[error("the file's path below the store's root is empty")]
    EmptyPath,
    #[error("a segment of the file's path is not UTF-8")]
    NotUtf8,
    #[error("a segment of the file's path cannot be part of an object's name")]
    InvalidPart(#[source] InvalidPart),
}

/// Returns the object `<prefix>/<rel's directories>/<store>.<rel's file>`, `rel` being a data
/// file's path relative to the store's root: the bucket's partition paths are the local ones,
/// segment for segment, percent-encoded tenant and all.
fn object_key(
    prefix: &ObjectPath,
    store: StoreId,
    rel: &Path,
) -> Result<ObjectPath, ObjectKeyError> {
    let segments: Vec<&str> = rel
        .iter()
        .map(|s| s.to_str().ok_or(ObjectKeyError::NotUtf8))
        .collect::<Result<_, _>>()?;
    let Some((file, dirs)) = segments.split_last() else {
        return Err(ObjectKeyError::EmptyPath);
    };
    let object = format!("{}.{file}", store.0);
    dirs.iter()
        .copied()
        .chain([object.as_str()])
        .try_fold(prefix.clone(), |key, segment| {
            Ok(key.join(PathPart::parse(segment).map_err(ObjectKeyError::InvalidPart)?))
        })
}

/// Returns what a create-only upload says of the bucket: an object already there under the name
/// is this file, uploaded by an earlier try or by another writer of the same store, so it counts
/// as landed.
fn landed(put: object_store::Result<PutResult>) -> object_store::Result<()> {
    match put {
        Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: StoreId = StoreId(Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef));

    #[test]
    fn an_object_is_named_by_its_local_partition_path_and_the_store_id() {
        let prefix = ObjectPath::from("prod");
        let rel = Path::new("tenant=a%40b/llm_call/date=2026-11-07/L2-00ff.parquet");

        let key = object_key(&prefix, ID, rel).unwrap();

        assert_eq!(
            key.as_ref(),
            "prod/tenant=a%40b/llm_call/date=2026-11-07/\
             01234567-89ab-cdef-0123-456789abcdef.L2-00ff.parquet",
            "the tenant's percent-encoding is kept as on disk"
        );
        assert_eq!(
            object_key(&ObjectPath::ROOT, ID, Path::new("f.parquet"))
                .unwrap()
                .as_ref(),
            "01234567-89ab-cdef-0123-456789abcdef.f.parquet"
        );
    }

    #[test]
    fn a_segment_that_cannot_name_an_object_is_refused() {
        let prefix = ObjectPath::ROOT;
        assert!(object_key(&prefix, ID, Path::new("tenant=x/t/date=d/f.parquet")).is_ok());
        assert!(matches!(
            object_key(&prefix, ID, Path::new("tenant=\u{7}/t/date=d/f.parquet")),
            Err(ObjectKeyError::InvalidPart(_))
        ));
        assert!(matches!(
            object_key(&prefix, ID, Path::new("")),
            Err(ObjectKeyError::EmptyPath)
        ));
    }

    #[test]
    fn an_upload_that_finds_the_object_there_has_landed_and_any_other_failure_has_not() {
        let exists = object_store::Error::AlreadyExists {
            path: "a".into(),
            source: "exists".into(),
        };
        let refused = object_store::Error::PermissionDenied {
            path: "a".into(),
            source: "denied".into(),
        };
        let put = PutResult {
            e_tag: None,
            version: None,
            extensions: Default::default(),
        };

        assert!(landed(Ok(put)).is_ok());
        assert!(landed(Err(exists)).is_ok());
        assert!(matches!(
            landed(Err(refused)),
            Err(object_store::Error::PermissionDenied { .. })
        ));
    }

    const TEN: NonZeroU64 = NonZeroU64::new(10).expect("10 is not zero");

    #[test]
    fn a_file_of_one_part_is_one_put_and_a_byte_more_is_sent_in_parts() {
        assert_eq!(transfer(0, TEN), Transfer::OnePut);
        assert_eq!(transfer(10, TEN), Transfer::OnePut);
        assert_eq!(transfer(11, TEN), Transfer::Parts);
    }

    fn file_of(bytes: &[u8]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        file
    }

    async fn object_bytes(store: &object_store::memory::InMemory, key: &ObjectPath) -> Vec<u8> {
        store
            .get_opts(key, Default::default())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn a_file_longer_than_one_part_lands_whole_in_the_bucket() {
        let store = object_store::memory::InMemory::new();
        let key = ObjectPath::from("d/f.parquet");
        let content: Vec<u8> = (0..25).collect();
        let file = file_of(&content);

        send_file(&store, &key, file.path(), TEN).await.unwrap();

        assert_eq!(object_bytes(&store, &key).await, content);
    }

    #[tokio::test]
    async fn a_file_whose_object_is_there_is_not_sent_again() {
        let store = object_store::memory::InMemory::new();
        let key = ObjectPath::from("d/f.parquet");
        store
            .put_opts(
                &key,
                PutPayload::from_static(b"earlier"),
                PutMode::Create.into(),
            )
            .await
            .unwrap();

        for len in [5, 25] {
            let file = file_of(&vec![1; len]);
            send_file(&store, &key, file.path(), TEN).await.unwrap();
            assert_eq!(object_bytes(&store, &key).await, b"earlier", "{len} bytes");
        }
    }

    fn no_env() -> Vec<(OsString, OsString)> {
        Vec::new()
    }

    fn env(vars: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        vars.iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn a_bucket_client_reads_only_the_variables_under_its_sdks_prefix() {
        let found = env(&[
            ("ENDPOINT", "http://elsewhere"),
            ("TOKEN", "a token for another program"),
            ("AWS_REGION", "eu-west-1"),
            ("GOOGLE_BUCKET", "g"),
        ]);

        assert_eq!(
            sdk_variables(sdk_prefix(&ObjectStoreScheme::AmazonS3), found.clone()),
            vec![("AWS_REGION".to_owned(), "eu-west-1".to_owned())]
        );
        assert_eq!(
            sdk_variables(sdk_prefix(&ObjectStoreScheme::Local), found),
            vec![]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_variable_that_is_not_utf8_is_skipped_not_a_panic() {
        use std::os::unix::ffi::OsStringExt;
        let found = vec![
            (
                OsString::from("AWS_X"),
                OsString::from_vec(vec![0xff, 0xfe]),
            ),
            (OsString::from_vec(vec![b'A', 0xff]), OsString::from("v")),
            (OsString::from("AWS_REGION"), OsString::from("r")),
        ];

        assert_eq!(
            sdk_variables(Some("AWS_"), found),
            vec![("AWS_REGION".to_owned(), "r".to_owned())]
        );
    }

    #[cfg(feature = "object-store-cloud")]
    #[test]
    fn a_generic_variable_does_not_configure_an_s3_client() {
        let bad = |key: &str| Bucket::from_url("s3://bucket/prod", ID, env(&[(key, "not-a-bool")]));

        assert!(
            bad("ALLOW_HTTP").is_ok(),
            "a generic name is someone else's"
        );
        assert!(
            matches!(bad("AWS_ALLOW_HTTP"), Err(BucketSetupError::Client(_))),
            "the SDK's own variable is read"
        );
    }

    #[test]
    fn a_url_with_credentials_is_refused_without_repeating_them() {
        let refused = Bucket::from_url("s3://key:secret@bucket/prefix", ID, no_env()).unwrap_err();

        let said = refused.to_string();
        assert!(
            matches!(refused, BucketSetupError::UrlCredentials { .. }),
            "{said}"
        );
        assert!(!said.contains("secret") && !said.contains("key"), "{said}");
    }

    #[test]
    fn a_url_naming_no_known_store_is_refused() {
        assert!(matches!(
            Bucket::from_url("ftp://host/prefix", ID, no_env()),
            Err(BucketSetupError::Client(_))
        ));
        assert!(matches!(
            Bucket::from_url("not a url", ID, no_env()),
            Err(BucketSetupError::UrlUnparsed(_))
        ));
    }

    #[test]
    fn a_file_url_ships_into_that_directory() {
        let dir = tempfile::tempdir().unwrap();
        let url = Url::from_directory_path(dir.path()).unwrap();

        let target = Bucket::from_url(url.as_str(), ID, no_env()).unwrap();

        assert_eq!(
            format!("/{}", target.prefix.as_ref()),
            dir.path().to_str().unwrap().trim_end_matches('/')
        );
    }

    #[tokio::test]
    async fn a_directory_keeps_the_store_id_it_was_first_given() {
        let dir = tempfile::tempdir().unwrap();

        let first = StoreId::read_or_create(dir.path().to_path_buf())
            .await
            .unwrap();
        let again = StoreId::read_or_create(dir.path().to_path_buf())
            .await
            .unwrap();

        assert_eq!(first, again);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![STORE_ID], "no staging file is left behind");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_first_opens_read_one_store_id() {
        let dir = tempfile::tempdir().unwrap();
        let open = || StoreId::read_or_create(dir.path().to_path_buf());

        let (a, b) = tokio::join!(open(), open());

        assert_eq!(a.unwrap(), b.unwrap());
    }

    #[tokio::test]
    async fn a_store_id_file_holding_no_uuid_says_what_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(STORE_ID), "garbled").unwrap();

        let refused = StoreId::read_or_create(dir.path().to_path_buf()).await;

        assert!(
            matches!(&refused, Err(BucketSetupError::StoreIdText { found, .. }) if found == "garbled"),
            "{refused:?}"
        );
    }
}
