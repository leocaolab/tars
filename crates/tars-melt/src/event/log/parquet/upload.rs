//! Uploads frozen files on a task of its own, so a bucket that is slow, down or refusing never
//! holds up the merges and cleanup of the sink's upkeep. The upkeep tells it which partition
//! directories to look in; what to upload there each manifest says. It decides when to try again
//! after a failure; it does not decide which files are frozen, and nothing waits on it but the
//! sink's `close()`, which lets it try what is queued once more.

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::super::sink::count_upkeep_failure;
use super::super::{LogError, SinkHealth, TableName};
use super::IoWork;
use super::partition::PartitionDir;
use super::ship::{self, Bucket};

/// The wait after the first failed pass; each further failure in a row doubles it.
const FIRST_RETRY: Duration = Duration::from_secs(1);
/// The longest wait, and the wait after a failure no retry alone can mend.
const LAST_RETRY: Duration = Duration::from_secs(600);

/// A sink's upload task and the queue of directories it looks in.
pub(super) struct Uploader {
    queue: mpsc::UnboundedSender<PartitionDir>,
    task: JoinHandle<()>,
    root: PathBuf,
    health: Arc<Mutex<SinkHealth>>,
}

impl Uploader {
    /// Spawns on `rt` the task uploading the frozen files of `root`'s partitions to `bucket`,
    /// counting its failures in `health` as upkeep failures of `table`.
    pub(super) fn spawn(
        root: PathBuf,
        bucket: Bucket,
        table: TableName,
        health: Arc<Mutex<SinkHealth>>,
        rt: &Handle,
    ) -> Self {
        let (queue, queued) = mpsc::unbounded_channel();
        let task = rt.spawn(run(
            root.clone(),
            bucket,
            table,
            Arc::clone(&health),
            queued,
        ));
        Self {
            queue,
            task,
            root,
            health,
        }
    }

    /// Asks the task to upload what `dirs` hold frozen and unconfirmed.
    pub(super) fn queue(&self, dirs: impl IntoIterator<Item = PartitionDir>) {
        for dir in dirs {
            // [arc:intentional-handle] reason: the receiver goes only when the task ends, which
            // before `finish` means it panicked; `finish` reports that, and the directory is
            // queued again by the next sink's sweep.
            if self.queue.send(dir).is_err() {
                return;
            }
        }
    }

    /// Lets the task try every queued directory once more, without waiting out a backoff, and
    /// waits for it to end. A task that panicked is counted as an upkeep failure.
    pub(super) async fn finish(self) {
        drop(self.queue);
        if let Err(source) = self.task.await {
            count_upkeep_failure(
                &self.health,
                LogError::IoTask {
                    work: IoWork::Upload(self.root),
                    source,
                },
            );
        }
    }
}

/// Whether a failed upload can succeed on a later try with nobody changing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Persistence {
    Transient,
    /// The bucket refused the credentials or the operation: only a person can mend it.
    Permanent,
}

fn persistence(e: &LogError) -> Persistence {
    match e {
        LogError::Upload { source, .. } => match **source {
            object_store::Error::PermissionDenied { .. }
            | object_store::Error::Unauthenticated { .. }
            | object_store::Error::NotImplemented { .. }
            | object_store::Error::NotSupported { .. } => Persistence::Permanent,
            _ => Persistence::Transient,
        },
        _ => Persistence::Transient,
    }
}

/// Returns how long to wait before the next pass once `streak` passes in a row have failed, the
/// last one `persistence`.
fn retry_delay(streak: NonZeroU32, persistence: Persistence) -> Duration {
    match persistence {
        Persistence::Permanent => LAST_RETRY,
        Persistence::Transient => {
            let doublings = streak.get() - 1;
            FIRST_RETRY
                .checked_mul(2_u32.saturating_pow(doublings))
                .map_or(LAST_RETRY, |wait| wait.min(LAST_RETRY))
        }
    }
}

/// Failed passes in a row, and when the next one is due.
#[derive(Debug, Clone, Copy)]
struct Failing {
    streak: NonZeroU32,
    retry_at: Instant,
}

enum Wake {
    Queued(PartitionDir),
    /// The sink is closing: one last pass, then the task ends.
    Closing,
    RetryDue,
}

async fn run(
    root: PathBuf,
    bucket: Bucket,
    table: TableName,
    health: Arc<Mutex<SinkHealth>>,
    mut queued_rx: mpsc::UnboundedReceiver<PartitionDir>,
) {
    let mut queued = BTreeSet::new();
    let mut failing: Option<Failing> = None;
    loop {
        let wake = tokio::select! {
            dir = queued_rx.recv() => dir.map_or(Wake::Closing, Wake::Queued),
            () = retry_due(failing) => Wake::RetryDue,
        };
        let closing = match wake {
            Wake::Queued(dir) => {
                queued.insert(dir);
                false
            }
            Wake::Closing => true,
            Wake::RetryDue => false,
        };
        while let Ok(dir) = queued_rx.try_recv() {
            queued.insert(dir);
        }
        if !closing && failing.is_some_and(|f| Instant::now() < f.retry_at) {
            continue;
        }

        let failures = pass(&root, &bucket, &mut queued).await;
        failing = after_pass(failing, failures, table, &health);
        if closing {
            return;
        }
    }
}

async fn retry_due(failing: Option<Failing>) {
    match failing {
        Some(f) => tokio::time::sleep_until(f.retry_at).await,
        None => std::future::pending().await,
    }
}

/// Uploads what each queued directory holds frozen and unconfirmed. A directory leaves the queue
/// once all of it is in the bucket; one that fails stays.
async fn pass(
    root: &std::path::Path,
    bucket: &Bucket,
    queued: &mut BTreeSet<PartitionDir>,
) -> Vec<LogError> {
    let mut failures = Vec::new();
    for dir in queued.clone() {
        match ship::upload_partition(root, dir.clone(), bucket).await {
            Ok(()) => {
                queued.remove(&dir);
            }
            Err(failed) => failures.push(failed),
        }
    }
    failures
}

/// Counts the pass's failures and returns when the next is due. A failure that starts a streak is
/// logged; the rest of the streak is only counted, so a bucket that stays down says so once.
fn after_pass(
    failing: Option<Failing>,
    failures: Vec<LogError>,
    table: TableName,
    health: &Mutex<SinkHealth>,
) -> Option<Failing> {
    if failures.is_empty() {
        return None;
    }
    let persisting = if failures
        .iter()
        .any(|e| persistence(e) == Persistence::Permanent)
    {
        Persistence::Permanent
    } else {
        Persistence::Transient
    };
    let streak = failing.map_or(NonZeroU32::MIN, |f| f.streak.saturating_add(1));
    for error in failures {
        if streak == NonZeroU32::MIN {
            tracing::warn!(
                table = table.as_str(),
                error = %tars_utils::display_chain(&error),
                "upload to the bucket failed; retrying with backoff"
            );
        }
        count_upkeep_failure(health, error);
    }
    Some(Failing {
        streak,
        retry_at: Instant::now() + retry_delay(streak, persisting),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streak(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    #[test]
    fn a_transient_failure_backs_off_doubling_up_to_the_cap() {
        let wait = |n| retry_delay(streak(n), Persistence::Transient);

        assert_eq!(wait(1), FIRST_RETRY);
        assert_eq!(wait(2), FIRST_RETRY * 2);
        assert_eq!(wait(10), FIRST_RETRY * 512);
        assert_eq!(wait(11), LAST_RETRY, "1024 s is past the cap");
        assert_eq!(wait(u32::MAX), LAST_RETRY);
    }

    #[test]
    fn a_failure_only_a_person_can_mend_waits_the_longest_at_once() {
        assert_eq!(retry_delay(streak(1), Persistence::Permanent), LAST_RETRY);
    }

    #[test]
    fn a_refused_credential_is_permanent_and_a_network_error_is_not() {
        let upload = |source| LogError::Upload {
            path: PathBuf::from("f"),
            source: Box::new(source),
        };
        let denied = object_store::Error::PermissionDenied {
            path: "f".into(),
            source: "denied".into(),
        };
        let generic = object_store::Error::Generic {
            store: "S3",
            source: "connection reset".into(),
        };

        assert_eq!(persistence(&upload(denied)), Persistence::Permanent);
        assert_eq!(persistence(&upload(generic)), Persistence::Transient);
    }
}
