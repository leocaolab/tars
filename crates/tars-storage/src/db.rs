//! Opens the SQLite database a store is built from and runs the store's migrations on it.
//!
//! How a file is opened (created or not, read-only or not, journal mode, sync level, busy wait) is
//! decided here and nowhere else; a store states only which [`Access`] it needs. Queries are not
//! abstracted: they run on the pool [`Db::pool`] returns, and both the queries and the migration
//! files are SQLite dialect (`?` placeholders, `INSERT OR REPLACE`, `PRAGMA`, `STRICT`).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use thiserror::Error;

/// How long a statement waits for a lock another connection holds before SQLite returns
/// `SQLITE_BUSY`, so a contended write waits instead of failing to a caller that would only
/// retry anyway.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Opening or migrating a database failed.
#[derive(Debug, Error)]
pub enum DbError {
    #[error("open {} ({access})", path.display())]
    Open {
        path: PathBuf,
        access: Access,
        #[source]
        source: sqlx::Error,
    },

    #[error("open :memory:")]
    OpenInMemory(#[source] sqlx::Error),

    #[error("migrate {at}")]
    Migrate {
        at: Location,
        #[source]
        source: sqlx::migrate::MigrateError,
    },
}

/// Returns whether `e` is SQLite's `SQLITE_BUSY` or `SQLITE_LOCKED` (in any extended form):
/// another connection held the database when this one needed it, so the same call can pass on
/// another try.
///
/// A pool that ran out of time waiting for a free connection is not counted: the caller itself
/// may hold every connection, and then no other try passes.
pub fn is_lock_contention(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Database(db) => db
            .code()
            .and_then(|c| c.parse::<i64>().ok())
            .is_some_and(is_contended),
        _ => false,
    }
}

/// Returns whether an SQLite result code (primary or extended) is `SQLITE_BUSY` (5) or
/// `SQLITE_LOCKED` (6): the extended codes keep the primary code in their low byte.
pub(crate) fn is_contended(code: i64) -> bool {
    const BUSY: i64 = 5;
    const LOCKED: i64 = 6;
    matches!(code & 0xff, BUSY | LOCKED)
}

/// What a caller may do to the file it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Read and write, creating the file if it is missing: the caller's store owns the file.
    Create,
    /// Read and write a file that must already exist; nothing is created.
    Existing,
    /// Read a file that must already exist, opened `SQLITE_OPEN_READONLY`, so SQLite itself
    /// refuses every write.
    ReadOnly,
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Create => "create",
            Self::Existing => "existing",
            Self::ReadOnly => "read-only",
        })
    }
}

/// Where a [`Db`] lives.
#[derive(Clone, Debug)]
pub enum Location {
    File(PathBuf),
    Memory,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => write!(f, "{}", path.display()),
            Self::Memory => f.write_str(":memory:"),
        }
    }
}

/// Returns the connection settings for the file at `path` opened with `access`.
///
/// A writer gets WAL, so a reader never blocks it, and `synchronous = NORMAL`, because WAL
/// already survives a process crash and only a machine crash can lose the last commits. A
/// read-only handle sets no journal mode: switching one is a write, and the file may be another
/// program's.
pub(crate) fn connect_options(path: &Path, access: Access) -> SqliteConnectOptions {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .busy_timeout(BUSY_TIMEOUT);
    match access {
        Access::Create | Access::Existing => opts
            .create_if_missing(access == Access::Create)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .pragma("temp_store", "MEMORY"),
        Access::ReadOnly => opts.read_only(true),
    }
}

/// A database a store reads and writes.
///
/// A clone shares the pool, so the stores on one file share one pool.
#[derive(Clone, Debug)]
pub struct Db {
    pool: SqlitePool,
    location: Location,
}

impl Db {
    /// Opens the SQLite file at `path` with `access`.
    ///
    /// # Errors
    ///
    /// [`DbError::Open`] when SQLite cannot open the file: a missing file under
    /// [`Access::Existing`] or [`Access::ReadOnly`], a missing directory, a file that is not a
    /// database, or a lock another connection holds past the busy wait.
    pub async fn open(path: impl AsRef<Path>, access: Access) -> Result<Self, DbError> {
        let path = path.as_ref();
        let pool = SqlitePoolOptions::new()
            .connect_with(connect_options(path, access))
            .await
            .map_err(|source| DbError::Open {
                path: path.to_path_buf(),
                access,
                source,
            })?;
        Ok(Self {
            pool,
            location: Location::File(path.to_path_buf()),
        })
    }

    /// Opens an in-memory SQLite database, for tests and ephemeral use.
    ///
    /// The pool holds exactly one connection for as long as it lives: an in-memory database
    /// belongs to its connection, so a second connection, or one sqlx would open after closing
    /// an idle or old one, is a second, empty database.
    ///
    /// # Errors
    ///
    /// [`DbError::OpenInMemory`] when SQLite cannot open the database.
    pub async fn open_in_memory() -> Result<Self, DbError> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(DbError::OpenInMemory)?
            .pragma("temp_store", "MEMORY");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .map_err(DbError::OpenInMemory)?;
        Ok(Self {
            pool,
            location: Location::Memory,
        })
    }

    /// Applies a store's embedded migrations; sqlx records the applied versions in
    /// `_sqlx_migrations`.
    ///
    /// # Errors
    ///
    /// [`DbError::Migrate`] when a migration fails, the file records a version this migrator
    /// does not know, or the database was opened [`Access::ReadOnly`].
    pub async fn migrate(&self, migrator: &sqlx::migrate::Migrator) -> Result<(), DbError> {
        migrator
            .run(&self.pool)
            .await
            .map_err(|source| DbError::Migrate {
                at: self.location.clone(),
                source,
            })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_keeps_one_connection_so_writes_do_not_vanish() {
        let db = Db::open_in_memory().await.unwrap();
        sqlx::query("CREATE TABLE t (x INTEGER)")
            .execute(db.pool())
            .await
            .unwrap();
        // A second connection would be a second, empty database — this INSERT and
        // the SELECT below have to land on the same one.
        sqlx::query("INSERT INTO t (x) VALUES (1)")
            .execute(db.pool())
            .await
            .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn in_memory_never_closes_its_connection_for_idling_or_age() {
        let db = Db::open_in_memory().await.unwrap();
        let options = db.pool().options();
        assert_eq!(options.get_idle_timeout(), None);
        assert_eq!(options.get_max_lifetime(), None);
    }

    #[tokio::test]
    async fn a_clone_shares_the_pool_rather_than_opening_a_second_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db"), Access::Create)
            .await
            .unwrap();
        let cloned = db.clone();
        sqlx::query("CREATE TABLE t (x INTEGER)")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO t (x) VALUES (1)")
            .execute(cloned.pool())
            .await
            .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(n, 1, "a clone must see the original's schema and rows");
    }

    #[tokio::test]
    async fn opening_a_file_wal_lets_a_reader_and_a_writer_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db"), Access::Create)
            .await
            .unwrap();
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[tokio::test]
    async fn open_names_the_path_it_could_not_open() {
        let err = Db::open("/nonexistent-dir-for-this-test/t.db", Access::Create)
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Open { .. }), "{err:?}");
        assert!(
            format!("{err}").contains("nonexistent-dir-for-this-test"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn existing_and_read_only_create_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.db");
        for access in [Access::Existing, Access::ReadOnly] {
            let err = Db::open(&path, access).await.unwrap_err();
            assert!(matches!(err, DbError::Open { .. }), "{access}: {err:?}");
        }
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn read_only_refuses_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let writer = Db::open(&path, Access::Create).await.unwrap();
        sqlx::query("CREATE TABLE t (x INTEGER)")
            .execute(writer.pool())
            .await
            .unwrap();

        let reader = Db::open(&path, Access::ReadOnly).await.unwrap();
        let err = sqlx::query("INSERT INTO t (x) VALUES (1)")
            .execute(reader.pool())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("readonly"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_migration_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        Db::open(&path, Access::Create).await.unwrap();

        let err = Db::open(&path, Access::ReadOnly)
            .await
            .unwrap()
            .migrate(&sqlx::migrate::Migrator::DEFAULT)
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Migrate { .. }), "{err:?}");
        assert!(format!("{err}").contains("t.db"), "{err}");
    }
}

#[cfg(test)]
mod contention_tests {
    use std::time::Duration;

    use sqlx::sqlite::SqliteConnectOptions;

    use super::{is_contended, is_lock_contention};

    #[test]
    fn busy_and_locked_are_contention_in_every_extended_form() {
        for code in [5, 6, 261, 517, 773, 262] {
            assert!(is_contended(code), "{code}");
        }
    }

    #[test]
    fn other_codes_are_not_contention() {
        for code in [1, 4, 7, 8, 11, 14, 19, 2067] {
            assert!(!is_contended(code), "{code}");
        }
    }

    #[tokio::test]
    async fn the_busy_error_of_a_write_lock_another_connection_holds_is_contention() {
        use sqlx::Connection;

        let dir = tempfile::tempdir().unwrap();
        let options = SqliteConnectOptions::new()
            .filename(dir.path().join("held.db"))
            .create_if_missing(true)
            .busy_timeout(Duration::ZERO);
        let mut held = sqlx::SqliteConnection::connect_with(&options)
            .await
            .unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut held)
            .await
            .unwrap();
        let mut other = sqlx::SqliteConnection::connect_with(&options)
            .await
            .unwrap();

        let busy = sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut other)
            .await
            .unwrap_err();

        assert!(is_lock_contention(&busy), "{busy:?}");
    }

    #[test]
    fn a_pool_that_timed_out_is_not_contention() {
        assert!(!is_lock_contention(&sqlx::Error::PoolTimedOut));
    }
}
