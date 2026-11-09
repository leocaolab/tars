//! The shell every log adapter shares: a bounded channel into a background
//! task that buffers rows and seals them when the pure core says so, and
//! beside it an upkeep loop doing what those seals leave behind. What varies
//! per adapter is where a sealed batch goes and what its upkeep is
//! ([`SealWriter`]); a failed upkeep lands in [`SinkHealth`], never in the
//! lost-row count.
//!
//! Upkeep never runs on the sealing path: a long merge would stop the
//! channel from draining, and a full channel drops `try_append`ed rows. The
//! upkeep a seal owes is posted to a one-slot mailbox instead, where owed
//! rounds fold into one while the upkeep loop is busy.
//!
//! Some upkeep falls due only once time has passed (a merged-away file
//! outliving its grace): a round hands that back as deferred upkeep, and the
//! upkeep loop runs it when its wait is over, whether or not anything was
//! sealed since.
//!
//! Time is tokio's clock, read here and handed to the core as a value;
//! tests pause and advance it. A writer says how long deferred upkeep waits, read on its own
//! clock; this loop waits it out on tokio's.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::seal::{Pending, RowGroup, SealPolicy, overflows, row_groups, seal_due};
use super::{AppendError, LogError, LogSink, LogTable, Loss, SinkHealth, UpkeepFailure};

/// Where a sealed batch goes, and the upkeep (merging, cleanup) the writer
/// does after it.
#[async_trait]
pub(crate) trait SealWriter<T: LogTable>: Send + Sync + 'static {
    /// The upkeep one seal leaves behind.
    type Due: Coalesce + Send + 'static;
    /// The upkeep a round leaves for once time has passed.
    type Deferred: Coalesce + Send + 'static;

    /// Does the upkeep due as the sink opens, before any row arrives.
    async fn upkeep_on_open(&self) -> UpkeepReport<Self::Deferred>;
    /// Writes the batch; `Ok` means it is visible to readers, and carries the upkeep the seal
    /// owes. It does no upkeep itself.
    async fn seal(&self, groups: Vec<RowGroup<T>>) -> Result<Self::Due, LogError>;
    async fn upkeep(&self, due: Self::Due) -> UpkeepReport<Self::Deferred>;
    /// Returns how long `due` waits from now, on this writer's clock.
    fn wait(&self, due: &Self::Deferred) -> Duration;
    /// Does deferred upkeep, once its wait is over.
    async fn upkeep_deferred(&self, due: Self::Deferred) -> UpkeepReport<Self::Deferred>;
    /// Ends what the writer runs beside the sink's loops, once both have ended; `close()` returns
    /// after it.
    async fn finish(self)
    where
        Self: Sized,
    {
    }
}

/// Owed upkeep that folds: two rounds owed are one round doing both.
pub(crate) trait Coalesce {
    fn and(self, later: Self) -> Self;
}

impl Coalesce for () {
    fn and(self, (): ()) -> Self {}
}

/// The failures of one round of upkeep, and the upkeep it leaves for
/// later. None of the failures lost a row — a lost row fails the seal
/// itself.
#[derive(Debug)]
pub(crate) struct UpkeepReport<D> {
    pub(crate) failures: Vec<LogError>,
    pub(crate) deferred: Option<D>,
}

impl<D> Default for UpkeepReport<D> {
    fn default() -> Self {
        Self {
            failures: Vec::new(),
            deferred: None,
        }
    }
}

impl<D> FromIterator<LogError> for UpkeepReport<D> {
    fn from_iter<I: IntoIterator<Item = LogError>>(failures: I) -> Self {
        Self {
            failures: failures.into_iter().collect(),
            deferred: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SinkConfig {
    pub seal: SealPolicy,
    /// Rows the channel holds before `append` waits and `try_append` drops.
    pub capacity: NonZeroUsize,
}

/// A [`LogSink`] that buffers rows on a background task and hands each sealed batch to the
/// adapter's writer.
pub struct BufferedSink<T: LogTable> {
    state: Mutex<SinkState<T::Row>>,
    health: Arc<Mutex<SinkHealth>>,
}

enum SinkState<R> {
    Open {
        tx: mpsc::Sender<R>,
        task: JoinHandle<()>,
    },
    Closed,
}

impl<T: LogTable> BufferedSink<T> {
    /// Spawns the sealer and the upkeep loop on `rt`, which is passed explicitly because a caller
    /// on a foreign thread (tars-py) has no current runtime. `health` is where the sink counts
    /// what it lost and what upkeep failed; a writer may count its own failures there too.
    pub(crate) fn spawn<W: SealWriter<T>>(
        writer: W,
        health: Arc<Mutex<SinkHealth>>,
        config: SinkConfig,
        rt: &Handle,
    ) -> Self {
        let (tx, rx) = mpsc::channel(config.capacity.get());
        let task = rt.spawn(run_writer(rx, writer, config.seal, Arc::clone(&health)));
        Self {
            state: Mutex::new(SinkState::Open { tx, task }),
            health,
        }
    }

    fn sender(&self) -> Result<mpsc::Sender<T::Row>, AppendError> {
        match &*lock(&self.state) {
            SinkState::Open { tx, .. } => Ok(tx.clone()),
            SinkState::Closed => Err(AppendError::Closed),
        }
    }
}

#[async_trait]
impl<T: LogTable> LogSink<T> for BufferedSink<T> {
    async fn append(&self, row: T::Row) -> Result<(), AppendError> {
        let tx = self.sender()?;
        tx.send(row).await.map_err(|_| AppendError::Closed)
    }

    fn try_append(&self, row: T::Row) -> Result<(), AppendError> {
        let tx = self.sender()?;
        match tx.try_send(row) {
            Ok(()) => Ok(()),
            Err(TrySendError::Closed(_)) => Err(AppendError::Closed),
            Err(TrySendError::Full(_)) => {
                let full = AppendError::Full {
                    capacity: tx.max_capacity(),
                };
                record_loss(&self.health, 1, LogError::Append(full));
                Err(full)
            }
        }
    }

    async fn close(&self) -> Result<(), LogError> {
        let state = std::mem::replace(&mut *lock(&self.state), SinkState::Closed);
        if let SinkState::Open { tx, task } = state {
            drop(tx);
            task.await?;
        }

        match self.health().dropped {
            None => Ok(()),
            Some(loss) => Err(LogError::Dropped(loss)),
        }
    }

    fn health(&self) -> SinkHealth {
        lock(&self.health).clone()
    }
}

enum Wake<R> {
    Row(R),
    Closed,
    Deadline,
}

/// Runs the sealer and the upkeep loop side by side, then the writer's finish; all their IO is off
/// the async workers, so neither loop waits on the other's.
async fn run_writer<T: LogTable, W: SealWriter<T>>(
    rx: mpsc::Receiver<T::Row>,
    writer: W,
    policy: SealPolicy,
    health: Arc<Mutex<SinkHealth>>,
) {
    let (upkeep_tx, upkeep_rx) = mailbox();
    tokio::join!(
        run_sealer(rx, &writer, policy, &health, upkeep_tx),
        run_upkeep::<T, W>(&writer, &health, upkeep_rx),
    );
    writer.finish().await;
}

/// Seals rows until the channel closes and what is left is sealed; dropping `upkeep_tx` then lets the
/// upkeep loop finish too.
async fn run_sealer<T: LogTable, W: SealWriter<T>>(
    mut rx: mpsc::Receiver<T::Row>,
    writer: &W,
    policy: SealPolicy,
    health: &Mutex<SinkHealth>,
    upkeep_tx: UpkeepSender<W::Due>,
) {
    let mut pending = Pending::new();
    loop {
        let deadline = pending.deadline(&policy).map(Instant::from_std);
        let wake = tokio::select! {
            row = rx.recv() => row.map_or(Wake::Closed, Wake::Row),
            () = sleep_until(deadline) => Wake::Deadline,
        };

        let now = Instant::now().into_std();
        match wake {
            Wake::Row(row) => {
                let bytes = T::approx_bytes(&row);
                if overflows(&pending, bytes, &policy) {
                    seal(writer, &mut pending, health, &upkeep_tx).await;
                }
                pending.push(row, bytes, now);
                if seal_due(&pending, now, &policy) {
                    seal(writer, &mut pending, health, &upkeep_tx).await;
                }
            }
            Wake::Deadline => {
                if seal_due(&pending, now, &policy) {
                    seal(writer, &mut pending, health, &upkeep_tx).await;
                }
            }
            Wake::Closed => {
                if !pending.is_empty() {
                    seal(writer, &mut pending, health, &upkeep_tx).await;
                }
                return;
            }
        }
    }
}

/// Runs the upkeep due on open, then each round the seals owe or a deferral falls due for, until
/// the sealer is gone and nothing is owed. What is still deferred then is left to the next sink:
/// a closing sink does not wait out a grace.
async fn run_upkeep<T: LogTable, W: SealWriter<T>>(
    writer: &W,
    health: &Mutex<SinkHealth>,
    mut upkeep_rx: UpkeepReceiver<W::Due>,
) {
    let mut round = writer.upkeep_on_open().await;
    let mut waiting = None;
    loop {
        record_upkeep::<T>(health, round.failures);
        waiting = defer(waiting, round.deferred.map(|due| arrived(writer, due)));

        round = tokio::select! {
            owed = upkeep_rx.next() => match owed {
                Some(due) => writer.upkeep(due).await,
                None => return,
            },
            due = deferred(&mut waiting) => writer.upkeep_deferred(due).await,
        };
    }
}

/// A deferral as the upkeep loop holds it: due at a point on tokio's clock.
struct Waiting<D> {
    at: Instant,
    due: D,
}

/// Returns `due` placed on tokio's clock. The writer reads its own clock before this reads
/// tokio's, so the wait ends no earlier than the due time the writer measured.
fn arrived<T: LogTable, W: SealWriter<T>>(writer: &W, due: W::Deferred) -> Waiting<W::Deferred> {
    let wait = writer.wait(&due);
    Waiting {
        at: Instant::now() + wait,
        due,
    }
}

/// Returns `waiting` with `arrived` folded in, due at the earlier of the two.
fn defer<D: Coalesce>(
    waiting: Option<Waiting<D>>,
    arrived: Option<Waiting<D>>,
) -> Option<Waiting<D>> {
    match (waiting, arrived) {
        (Some(a), Some(b)) => Some(Waiting {
            at: a.at.min(b.at),
            due: a.due.and(b.due),
        }),
        (a, b) => a.or(b),
    }
}

/// Returns the waiting upkeep once it is due, and never while nothing waits.
async fn deferred<D>(waiting: &mut Option<Waiting<D>>) -> D {
    if let Some(at) = waiting.as_ref().map(|w| w.at) {
        tokio::time::sleep_until(at).await;
    }
    match waiting.take() {
        Some(w) => w.due,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn seal<T: LogTable, W: SealWriter<T>>(
    writer: &W,
    pending: &mut Pending<T::Row>,
    health: &Mutex<SinkHealth>,
    upkeep_tx: &UpkeepSender<W::Due>,
) {
    let rows = pending.take();
    let count = rows.len() as u64;
    match writer.seal(row_groups::<T>(rows)).await {
        Ok(due) => upkeep_tx.post(due),
        Err(e) => record_loss(health, count, e),
    }
}

/// The sealer's end of the mailbox: one slot of owed upkeep and a bell of
/// capacity one, so however far upkeep falls behind, what waits for it is
/// one folded round.
struct UpkeepSender<D> {
    slot: Arc<Mutex<Option<D>>>,
    ring: mpsc::Sender<()>,
}

/// The upkeep loop's end.
struct UpkeepReceiver<D> {
    slot: Arc<Mutex<Option<D>>>,
    rung: mpsc::Receiver<()>,
}

fn mailbox<D>() -> (UpkeepSender<D>, UpkeepReceiver<D>) {
    let slot = Arc::new(Mutex::new(None));
    let (ring, rung) = mpsc::channel(1);
    let upkeep_tx = UpkeepSender {
        slot: Arc::clone(&slot),
        ring,
    };
    (upkeep_tx, UpkeepReceiver { slot, rung })
}

impl<D: Coalesce> UpkeepSender<D> {
    /// Folds `due` into the slot before ringing, so a ring the upkeep loop has not taken yet finds
    /// it there.
    fn post(&self, due: D) {
        {
            let mut slot = lock(&self.slot);
            let folded = match slot.take() {
                None => due,
                Some(prev) => prev.and(due),
            };
            *slot = Some(folded);
        }
        // Full: a ring is already waiting and will find the slot. Closed
        // cannot happen: the upkeep loop only ends after this sender drops.
        match self.ring.try_send(()) {
            Ok(()) | Err(TrySendError::Full(()) | TrySendError::Closed(())) => {}
        }
    }
}

impl<D> UpkeepReceiver<D> {
    /// Returns the next folded round owed, or `None` once the sealer is gone and the slot is
    /// empty.
    async fn next(&mut self) -> Option<D> {
        loop {
            let rung = self.rung.recv().await;
            let due = lock(&self.slot).take();
            match (due, rung) {
                (Some(due), _) => return Some(due),
                (None, Some(())) => continue,
                (None, None) => return None,
            }
        }
    }
}

fn record_loss(health: &Mutex<SinkHealth>, rows: u64, cause: LogError) {
    let mut h = lock(health);
    let rows = match h.dropped.take() {
        None => rows,
        Some(prev) => prev.rows + rows,
    };
    h.dropped = Some(Loss {
        rows,
        last: Arc::new(cause),
    });
}

fn record_upkeep<T: LogTable>(health: &Mutex<SinkHealth>, failures: Vec<LogError>) {
    failures.into_iter().for_each(|error| {
        tracing::warn!(
            table = T::NAME.as_str(),
            error = %tars_utils::display_chain(&error),
            "log upkeep failed"
        );
        count_upkeep_failure(health, error);
    });
}

/// Counts `error` among the sink's upkeep failures, as the last one, without logging it.
pub(crate) fn count_upkeep_failure(health: &Mutex<SinkHealth>, error: LogError) {
    let mut h = lock(health);
    let times = match h.upkeep_failed.take() {
        None => 1,
        Some(prev) => prev.times + 1,
    };
    h.upkeep_failed = Some(UpkeepFailure {
        times,
        last: Arc::new(error),
    });
}

/// Locks `m`, taking the data from a poisoned lock: another thread panicked mid-update of plain
/// counters and handles, and the data is still the best truth there is.
pub(super) fn lock<X>(m: &Mutex<X>) -> MutexGuard<'_, X> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Coalesce for Vec<u32> {
        fn and(mut self, later: Self) -> Self {
            self.extend(later);
            self
        }
    }

    #[tokio::test]
    async fn owed_upkeep_folds_while_the_loop_is_busy_and_outlives_the_sealer() {
        let (upkeep_tx, mut upkeep_rx) = mailbox::<Vec<u32>>();

        upkeep_tx.post(vec![1]);
        upkeep_tx.post(vec![2]);
        upkeep_tx.post(vec![3]);
        assert_eq!(
            upkeep_rx.next().await,
            Some(vec![1, 2, 3]),
            "one folded round"
        );

        upkeep_tx.post(vec![4]);
        drop(upkeep_tx);
        assert_eq!(
            upkeep_rx.next().await,
            Some(vec![4]),
            "owed before the sealer went"
        );
        assert_eq!(upkeep_rx.next().await, None);
    }
}
