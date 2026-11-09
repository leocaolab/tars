//! Opens an event-store directory, the one way to reach its logs, so every caller reaches the
//! `llm_call`, `eval_score` and `llm_wire` logs through [`EventLogs::open_in_dir`] rather than
//! opening a [`ParquetLog`] itself.

use std::path::{Path, PathBuf};

use super::call_log::{EvalScoreTable, LlmCallTable};
use super::log::{Bucket, BucketSetupError, ParquetLog, ParquetTable, Partitioned, StoreId};
use super::wire::WireTable;

/// An event-store directory.
#[derive(Debug)]
pub struct EventLogs {
    dir: PathBuf,
    /// Where the sinks of the logs opened from here upload the past days' files.
    bucket: Option<Bucket>,
}

impl EventLogs {
    /// Opens the event store rooted at `dir` itself, with no subdirectory appended. A directory
    /// that does not exist opens as an empty store; creating it is the caller's call.
    pub fn open_in_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            bucket: None,
        }
    }

    /// Makes every sink of the logs opened from here upload each past day's files, once merged
    /// and frozen, to the bucket at `url` (`s3://bucket/prefix`, `gs://…`, `az://…`,
    /// `file:///…`). Credentials come from the SDK's own environment variables (`AWS_*`,
    /// `GOOGLE_*`, `AZURE_*`), never from the URL or any other variable. The store's id in the bucket is read from `<dir>/_store_id`, which the first call
    /// creates. Which tables upload is the caller's: a log whose sink it never opens uploads
    /// nothing.
    ///
    /// # Errors
    ///
    /// A URL that does not parse, carries credentials or names a store this build has no client
    /// for, and a store id that cannot be read or created.
    pub async fn ship_to(self, url: &str) -> Result<Self, BucketSetupError> {
        let store_id = StoreId::read_or_create(self.dir.clone()).await?;
        let target = Bucket::from_url(url, store_id, std::env::vars_os())?;
        Ok(Self {
            bucket: Some(target),
            ..self
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn calls(&self) -> ParquetLog<LlmCallTable> {
        self.log()
    }

    pub fn scores(&self) -> ParquetLog<EvalScoreTable> {
        self.log()
    }

    pub fn wire(&self) -> ParquetLog<WireTable> {
        self.log()
    }

    fn log<T: ParquetTable + Partitioned>(&self) -> ParquetLog<T> {
        ParquetLog::open(&self.dir).uploading_to(self.bucket.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shipping_to_a_bucket_gives_the_directory_one_store_id() {
        let dir = tempfile::tempdir().unwrap();
        let bucket = tempfile::tempdir().unwrap();
        let url = url::Url::from_directory_path(bucket.path()).unwrap();
        let open = || async {
            EventLogs::open_in_dir(dir.path())
                .ship_to(url.as_str())
                .await
                .unwrap()
        };

        let first = open().await;
        let id = std::fs::read_to_string(dir.path().join("_store_id")).unwrap();
        let again = open().await;

        assert!(first.bucket.is_some() && again.bucket.is_some());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("_store_id")).unwrap(),
            id
        );
    }

    #[tokio::test]
    async fn a_bucket_url_carrying_credentials_does_not_open() {
        let dir = tempfile::tempdir().unwrap();
        let stores = EventLogs::open_in_dir(dir.path());

        let refused = stores.ship_to("s3://key:secret@bucket/prod").await;

        assert!(
            matches!(refused, Err(BucketSetupError::UrlCredentials { .. })),
            "{refused:?}"
        );
    }
}
