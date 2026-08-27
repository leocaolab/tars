//! [`Db`] — the database handle every store is built from, and the one place in
//! this workspace that names a driver.
//!
//! # Why it exists
//!
//! Before this, four crates each carried a byte-identical `pool.rs` (same md5)
//! and seven stores each took a `SqlitePool` in their constructor. The comment on
//! those copies said they were kept separate so "no store crate gains a
//! cross-crate pool dependency" — which bought a dependency edge and paid four
//! copies of the WAL / synchronous / busy_timeout settings, where a fifth copy
//! that quietly disagreed would be a real difference nobody would see.
//!
//! # What it abstracts, and what it does not
//!
//! `Db` hides the driver at CONSTRUCTION: a store takes a `Db`, not a
//! `SqlitePool`. Migrations run through [`Db::migrate`], so the version-of-record
//! is one call rather than seven.
//!
//! It does NOT hide the driver at EXECUTION. Queries run against a concrete pool
//! reached through [`Db::sqlite`] — a deliberately named escape hatch, not an
//! oversight. `grep -c 'sqlite()'` across the workspace is exactly the surface
//! that is still driver-bound, and it is meant to be countable rather than hidden
//! behind a plausible-looking generic that would have to be unpicked later.
//!
//! Two further things stay driver-bound and are stated here so nobody plans
//! around a capability that is missing: the SQL carries `?` placeholders and
//! SQLite-only forms (`INSERT OR REPLACE`, `PRAGMA`), and the migration `.sql`
//! files are SQLite-dialect (`BLOB`, `AUTOINCREMENT`).

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use thiserror::Error;

/// Opening or migrating a database failed. Kept apart from the store errors: a
/// store that cannot open is a different fact from a store that opened and then
/// could not answer, and a caller retries them differently.
#[derive(Debug, Error)]
pub enum DbError {
    #[error("open {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: sqlx::Error,
    },

    #[error("migrate: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// A database a store reads and writes.
///
/// Cheap to clone — a clone shares the same underlying pool, which is the point:
/// one file gets one pool, and the stores that share the file share it.
#[derive(Clone, Debug)]
pub struct Db {
    backend: Backend,
}

#[derive(Clone, Debug)]
enum Backend {
    Sqlite(SqlitePool),
    // A second variant lands here, and the compiler then names every site that
    // has to answer for it — which is the reason this is an enum and not a
    // type alias.
}

impl Db {
    /// Open (creating if absent) a WAL SQLite file.
    ///
    /// WAL so a reader never blocks the writer; `synchronous = NORMAL` because
    /// WAL already survives a process crash and only a machine crash can lose
    /// the last commits; a 5s busy timeout so a contended write waits instead of
    /// returning `SQLITE_BUSY` to a caller that would only retry anyway.
    pub async fn open_sqlite(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref();
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .pragma("temp_store", "MEMORY");
        let pool =
            SqlitePoolOptions::new().connect_with(opts).await.map_err(|source| DbError::Open {
                path: path.display().to_string(),
                source,
            })?;
        Ok(Self { backend: Backend::Sqlite(pool) })
    }

    /// An in-memory SQLite database, for tests and ephemeral use.
    ///
    /// Pinned to ONE connection: an in-memory database belongs to its connection,
    /// so a second connection in the pool would be a second, empty database — and
    /// a test would see its own writes disappear at random.
    pub async fn sqlite_in_memory() -> Result<Self, DbError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(|source| DbError::Open { path: ":memory:".into(), source })?
            .pragma("temp_store", "MEMORY");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .connect_with(opts)
            .await
            .map_err(|source| DbError::Open { path: ":memory:".into(), source })?;
        Ok(Self { backend: Backend::Sqlite(pool) })
    }

    /// Apply a store's embedded migration set. The migrator's own
    /// `_sqlx_migrations` table is the version-of-record.
    pub async fn migrate(&self, migrator: &sqlx::migrate::Migrator) -> Result<(), DbError> {
        match &self.backend {
            Backend::Sqlite(pool) => migrator.run(pool).await?,
        }
        Ok(())
    }

    /// The SQLite pool, for executing queries.
    ///
    /// This is the escape hatch the module docs name: construction is abstract,
    /// execution is not. Every call site is a place that would have to answer for
    /// a second backend, so they are meant to be greppable and countable rather
    /// than hidden behind a plausible-looking generic.
    ///
    /// # Panics
    ///
    /// When the handle is not SQLite. That is unreachable today (there is one
    /// backend) and it is deliberately a panic rather than an `Option`: a caller
    /// handed `None` here could only give up, and a silent wrong answer from a
    /// store is worse than a crash that names the line.
    pub fn sqlite(&self) -> &SqlitePool {
        match &self.backend {
            Backend::Sqlite(pool) => pool,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_keeps_one_connection_so_writes_do_not_vanish() {
        let db = Db::sqlite_in_memory().await.unwrap();
        sqlx::query("CREATE TABLE t (x INTEGER)").execute(db.sqlite()).await.unwrap();
        // A second connection would be a second, empty database — this INSERT and
        // the SELECT below have to land on the same one.
        sqlx::query("INSERT INTO t (x) VALUES (1)").execute(db.sqlite()).await.unwrap();
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM t").fetch_one(db.sqlite()).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn a_clone_shares_the_pool_rather_than_opening_a_second_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_sqlite(dir.path().join("t.db")).await.unwrap();
        let cloned = db.clone();
        sqlx::query("CREATE TABLE t (x INTEGER)").execute(db.sqlite()).await.unwrap();
        sqlx::query("INSERT INTO t (x) VALUES (1)").execute(cloned.sqlite()).await.unwrap();
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM t").fetch_one(db.sqlite()).await.unwrap();
        assert_eq!(n, 1, "a clone must see the original's schema and rows");
    }

    #[tokio::test]
    async fn opening_a_file_wal_lets_a_reader_and_a_writer_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_sqlite(dir.path().join("t.db")).await.unwrap();
        let mode: String =
            sqlx::query_scalar("PRAGMA journal_mode").fetch_one(db.sqlite()).await.unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[tokio::test]
    async fn open_names_the_path_it_could_not_open() {
        let err = Db::open_sqlite("/nonexistent-dir-for-this-test/t.db").await.unwrap_err();
        assert!(matches!(err, DbError::Open { .. }), "{err:?}");
        assert!(format!("{err}").contains("nonexistent-dir-for-this-test"), "{err}");
    }
}
