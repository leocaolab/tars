//! In-memory adapter: each sealed batch is one "file" of sorted row groups,
//! the same layout the Parquet adapter writes, so a lookup prunes the same way.

use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use std::time::Duration;

use async_trait::async_trait;
use tokio::runtime::Handle;

use super::seal::RowGroup;
use super::sink::{BufferedSink, SealWriter, SinkConfig, UpkeepReport};
use super::{
    LogError, LogSource, LogTable, PartialSource, Partitioned, Projected, TenantScope,
    greatest_first,
};

type Files<T> = Vec<Vec<RowGroup<T>>>;

/// A log held in memory. Clones share the same sealed rows.
pub struct MemLog<T: LogTable> {
    files: Arc<RwLock<Files<T>>>,
}

impl<T: LogTable> Clone for MemLog<T> {
    fn clone(&self) -> Self {
        Self {
            files: Arc::clone(&self.files),
        }
    }
}

impl<T: LogTable> Default for MemLog<T> {
    fn default() -> Self {
        Self {
            files: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl<T: LogTable> MemLog<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a sink sealing into this log.
    pub fn sink(&self, config: SinkConfig, rt: &Handle) -> BufferedSink<T> {
        BufferedSink::spawn(self.clone(), Arc::default(), config, rt)
    }

    fn read(&self) -> RwLockReadGuard<'_, Files<T>> {
        self.files.read().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl<T: LogTable> SealWriter<T> for MemLog<T> {
    /// Memory needs no upkeep.
    type Due = ();
    type Deferred = ();

    async fn upkeep_on_open(&self) -> UpkeepReport<()> {
        UpkeepReport::default()
    }

    async fn seal(&self, groups: Vec<RowGroup<T>>) -> Result<(), LogError> {
        self.files
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(groups);
        Ok(())
    }

    async fn upkeep(&self, (): ()) -> UpkeepReport<()> {
        UpkeepReport::default()
    }

    fn wait(&self, (): &()) -> Duration {
        Duration::ZERO
    }

    async fn upkeep_deferred(&self, (): ()) -> UpkeepReport<()> {
        UpkeepReport::default()
    }
}

#[async_trait]
impl<T: LogTable> LogSource<T> for MemLog<T>
where
    T::Row: Clone,
{
    async fn scan(&self, scope: &TenantScope) -> Result<Vec<T::Row>, LogError>
    where
        T: Partitioned,
    {
        Ok(self
            .read()
            .iter()
            .flatten()
            .flat_map(|g| g.rows().iter().filter(|r| in_scope::<T>(r, scope)).cloned())
            .collect())
    }

    async fn scan_key(&self, scope: &TenantScope, key: &T::Key) -> Result<Vec<T::Row>, LogError>
    where
        T: Partitioned,
    {
        Ok(self
            .read()
            .iter()
            .flatten()
            .filter(|g| g.may_contain(key))
            .flat_map(|g| {
                g.rows()
                    .iter()
                    .filter(|r| T::sort_key(r) == *key && in_scope::<T>(r, scope))
                    .cloned()
            })
            .collect())
    }

    /// Memory reads no files: every row in scope is filtered, then cut.
    async fn scan_newest(
        &self,
        scope: &TenantScope,
        filter: &T::Filter,
        limit: Option<usize>,
    ) -> Result<Vec<T::Row>, LogError>
    where
        T: Projected,
    {
        let rows = self
            .read()
            .iter()
            .flatten()
            .flat_map(|g| {
                g.rows()
                    .iter()
                    .filter(|r| in_scope::<T>(r, scope) && T::keeps(filter, &T::partial(r)))
                    .cloned()
            })
            .collect();
        Ok(greatest_first(rows, T::sort_key, limit))
    }
}

#[async_trait]
impl<T: Projected> PartialSource<T> for MemLog<T> {
    /// Memory reads no files: every row in scope is summarized and
    /// filtered, then cut.
    async fn scan_partial(
        &self,
        scope: &TenantScope,
        filter: &T::Filter,
        limit: Option<usize>,
    ) -> Result<Vec<T::Partial>, LogError> {
        let partials = self
            .read()
            .iter()
            .flatten()
            .flat_map(|g| {
                g.rows()
                    .iter()
                    .filter(|r| in_scope::<T>(r, scope))
                    .map(T::partial)
                    .filter(|p| T::keeps(filter, p))
            })
            .collect();
        Ok(greatest_first(partials, T::partial_key, limit))
    }
}

/// Memory has no partition directories: a row is in scope when its own
/// partition is one the scope covers.
fn in_scope<T: Partitioned>(row: &T::Row, scope: &TenantScope) -> bool {
    let p = T::partition(row);
    &p.tenant == scope.tenant() && scope.window().covers(p.day)
}

#[cfg(test)]
mod tests {
    use super::super::contract::{Harness, TestTable, contract_suite};
    use super::*;
    use crate::event::log::{LogSink, LogSource};

    #[async_trait]
    impl Harness for MemLog<TestTable> {
        async fn sink(&self, config: SinkConfig) -> Result<Box<dyn LogSink<TestTable>>, LogError> {
            Ok(Box::new(MemLog::sink(self, config, &Handle::current())))
        }

        async fn source(&self) -> Result<Box<dyn LogSource<TestTable>>, LogError> {
            Ok(Box::new(self.clone()))
        }
    }

    contract_suite!(MemLog::<TestTable>::new());
}
