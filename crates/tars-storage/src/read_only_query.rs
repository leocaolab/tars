//! One read-only statement against a SQLite file this crate does not own.
//!
//! Every other store here owns its file: it creates it, migrates it, and knows
//! its schema. This one owns nothing. It is handed a path to somebody else's
//! database — a fiber journal, a cache, a `.db` an agent found on disk — and
//! reads it.
//!
//! # No migration, deliberately
//!
//! There is no [`sqlx::migrate!`] here and there must never be one. A migrator
//! writes: it creates `_sqlx_migrations` and applies DDL. Against a file we did
//! not create, that is wrong twice over — on a read-only handle it fails, and if
//! it ever succeeded it would have rewritten a database whose owner is a
//! different program. Nothing here knows the schema, and nothing here should:
//! `SELECT name, sql FROM sqlite_master` is how a caller finds out.
//!
//! # Read-only is the guard
//!
//! The file is opened `SQLITE_OPEN_READONLY`, so a write, a schema change or a
//! mutating `PRAGMA` is refused by SQLite in its own words. That is the whole
//! safety story — no keyword blocklist, because a blocklist is a thing to be
//! wrong about. The path goes in through `filename`, which takes it as a path,
//! so a caller cannot smuggle `file:…?mode=rwc` past the flag the way a URI
//! could.
//!
//! `immutable` is deliberately NOT set. A journal a live run is still writing
//! has its tail in the WAL; an immutable open reads the main file alone and
//! would report a truncated table as if it were the whole one.

use std::path::{Path, PathBuf};

use futures::TryStreamExt;
use sqlx::sqlite::{SqliteConnectOptions, SqliteValueRef};
use sqlx::{Column, ConnectOptions, Decode, Executor, Row, Sqlite, Statement, TypeInfo, ValueRef};
use thiserror::Error;

/// One cell, with SQLite's own type kept. A consumer branches on the variant
/// rather than re-parsing a string, and a blob arrives as its length because
/// its bytes are not text — rendering them would put mojibake on the page.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob { bytes: usize },
}

/// What one statement returned. `columns` is filled from the prepared
/// statement, so a query that matches nothing still says which columns it is
/// empty of.
#[derive(Clone, Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// The row cap was reached and reading stopped. Carried, never silent: a
    /// listing cut at the ceiling and a listing that IS the table are different
    /// facts, and a caller that cannot tell them apart will report the wrong one.
    pub truncated: bool,
}

/// The ways one read-only query fails, kept apart because callers act on them
/// differently — a syntax error is the caller's to fix, a decode failure is
/// ours, and a missing file is neither.
#[derive(Debug, Error)]
pub enum ReadOnlyQueryError {
    #[error("open {}: {source}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: sqlx::Error,
    },

    /// The statement did not prepare. SQLite's message names the token and the
    /// position, which is the actionable part — it travels whole.
    #[error("{0}")]
    Prepare(#[source] sqlx::Error),

    /// Preparing worked; running did not. A write refused by the read-only
    /// handle lands here, in the database's own words.
    #[error("{0}")]
    Run(#[source] sqlx::Error),

    #[error("column `{column}` declared {declared}: {source}")]
    Decode {
        column: String,
        declared: String,
        #[source]
        source: sqlx::error::BoxDynError,
    },
}

/// Run one statement against `path`, reading at most `row_cap` rows.
///
/// `row_cap` is a ceiling on what is read, not a `LIMIT` appended to the
/// caller's SQL: rows are streamed and reading stops, so a `SELECT *` over a
/// large journal costs the ceiling rather than the table.
pub async fn query_read_only(
    path: &Path,
    sql: &str,
    row_cap: usize,
) -> Result<QueryResult, ReadOnlyQueryError> {
    let opts = SqliteConnectOptions::new().filename(path).read_only(true);
    let mut conn = opts
        .connect()
        .await
        .map_err(|source| ReadOnlyQueryError::Open {
            path: path.to_path_buf(),
            source,
        })?;

    // Prepared before it is run, so the column list survives a query that
    // matches nothing.
    let stmt = conn
        .prepare(sql)
        .await
        .map_err(ReadOnlyQueryError::Prepare)?;
    let columns: Vec<String> = stmt
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();

    let mut rows = Vec::new();
    let mut truncated = false;
    {
        let mut cursor = stmt.query().fetch(&mut conn);
        while let Some(r) = cursor.try_next().await.map_err(ReadOnlyQueryError::Run)? {
            if rows.len() >= row_cap {
                truncated = true;
                break;
            }
            let mut out = Vec::with_capacity(columns.len());
            for (i, name) in columns.iter().enumerate() {
                let v = r.try_get_raw(i).map_err(ReadOnlyQueryError::Run)?;
                out.push(cell(v, name)?);
            }
            rows.push(out);
        }
    }

    Ok(QueryResult {
        columns,
        rows,
        truncated,
    })
}

fn cell(v: SqliteValueRef<'_>, column: &str) -> Result<Cell, ReadOnlyQueryError> {
    if v.is_null() {
        return Ok(Cell::Null);
    }
    // Read the type before decoding: `decode` consumes the ref, and the declared
    // type is what says which decode is the right one.
    let declared = v.type_info().name().to_ascii_uppercase();
    let failed = |source| ReadOnlyQueryError::Decode {
        column: column.to_string(),
        declared: declared.clone(),
        source,
    };
    Ok(match declared.as_str() {
        "INTEGER" => Cell::Integer(<i64 as Decode<Sqlite>>::decode(v).map_err(failed)?),
        "REAL" => Cell::Real(<f64 as Decode<Sqlite>>::decode(v).map_err(failed)?),
        "BLOB" => Cell::Blob {
            bytes: <&[u8] as Decode<Sqlite>>::decode(v).map_err(failed)?.len(),
        },
        // TEXT, and whatever else a view or an expression reports itself as: the
        // value is a string either way, and carrying it beats refusing over a
        // type name we did not anticipate.
        _ => Cell::Text(
            <&str as Decode<Sqlite>>::decode(v)
                .map_err(failed)?
                .to_string(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db_with(dir: &Path, name: &str, ddl: &str) -> PathBuf {
        let p = dir.join(name);
        let opts = SqliteConnectOptions::new()
            .filename(&p)
            .create_if_missing(true);
        let mut c = opts.connect().await.unwrap();
        c.execute(ddl).await.unwrap();
        p
    }

    const FIXTURE: &str =
        "CREATE TABLE t (seq INTEGER PRIMARY KEY, name TEXT, ratio REAL, body BLOB, absent TEXT);
         INSERT INTO t (name, ratio, body, absent) VALUES ('a', 1.5, x'DEADBEEF', NULL);
         INSERT INTO t (name, ratio, body, absent) VALUES ('b', 2.5, x'BEEF', NULL);";

    #[tokio::test]
    async fn sqlites_own_types_survive_the_trip() {
        let d = tempfile::tempdir().unwrap();
        let p = db_with(d.path(), "t.db", FIXTURE).await;
        let r = query_read_only(
            &p,
            "SELECT seq, name, ratio, body, absent FROM t ORDER BY seq",
            10,
        )
        .await
        .unwrap();
        assert_eq!(r.columns, ["seq", "name", "ratio", "body", "absent"]);
        assert_eq!(r.rows[0][0], Cell::Integer(1));
        assert_eq!(r.rows[0][1], Cell::Text("a".into()));
        assert_eq!(r.rows[0][2], Cell::Real(1.5));
        assert_eq!(r.rows[0][3], Cell::Blob { bytes: 4 });
        assert_eq!(r.rows[0][4], Cell::Null);
    }

    /// The cap is a ceiling on reading, and it says so.
    #[tokio::test]
    async fn the_row_cap_is_reported_not_silent() {
        let d = tempfile::tempdir().unwrap();
        let p = db_with(d.path(), "t.db", FIXTURE).await;
        let r = query_read_only(&p, "SELECT seq FROM t", 1).await.unwrap();
        assert_eq!(r.rows.len(), 1);
        assert!(r.truncated, "a listing cut at the ceiling must say so");

        let all = query_read_only(&p, "SELECT seq FROM t", 10).await.unwrap();
        assert!(
            !all.truncated,
            "a listing that IS the table must not claim it was cut"
        );
    }

    /// An empty answer still says which columns it is empty of.
    #[tokio::test]
    async fn no_matching_rows_still_carries_the_column_list() {
        let d = tempfile::tempdir().unwrap();
        let p = db_with(d.path(), "t.db", FIXTURE).await;
        let r = query_read_only(&p, "SELECT seq, name FROM t WHERE seq = 999", 10)
            .await
            .unwrap();
        assert!(r.rows.is_empty());
        assert_eq!(r.columns, ["seq", "name"]);
    }

    /// The guard is SQLite's, not a keyword list of ours — and nothing happened.
    #[tokio::test]
    async fn a_write_is_refused_by_the_database_itself() {
        let d = tempfile::tempdir().unwrap();
        let p = db_with(d.path(), "t.db", FIXTURE).await;
        let e = query_read_only(&p, "DELETE FROM t", 10)
            .await
            .expect_err("read-only");
        let msg = format!("{e:?}").to_lowercase();
        assert!(
            msg.contains("readonly") || msg.contains("read-only"),
            "{e:?}"
        );
        let after = query_read_only(&p, "SELECT seq FROM t", 10).await.unwrap();
        assert_eq!(
            after.rows.len(),
            2,
            "the refused write really did not happen"
        );
    }

    /// A malformed statement fails at prepare, carrying SQLite's own words —
    /// which is a different fact from a statement that ran and then failed.
    #[tokio::test]
    async fn a_syntax_error_is_a_prepare_failure_in_sqlites_words() {
        let d = tempfile::tempdir().unwrap();
        let p = db_with(d.path(), "t.db", FIXTURE).await;
        let e = query_read_only(&p, "SELECT FROM WHERE", 10)
            .await
            .expect_err("malformed");
        assert!(matches!(e, ReadOnlyQueryError::Prepare(_)), "{e:?}");
        assert!(format!("{e}").to_lowercase().contains("syntax"), "{e}");
    }

    /// Opening is its own failure, and it names the path.
    #[tokio::test]
    async fn a_missing_file_is_an_open_failure_that_names_it() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("nope.db");
        let e = query_read_only(&p, "SELECT 1", 10)
            .await
            .expect_err("absent");
        assert!(matches!(e, ReadOnlyQueryError::Open { .. }), "{e:?}");
        assert!(format!("{e}").contains("nope.db"), "{e}");
    }

    /// The schema is discoverable without knowing it first — which is the whole
    /// reason this reader may not carry a migrator.
    #[tokio::test]
    async fn sqlite_master_answers_what_tables_exist() {
        let d = tempfile::tempdir().unwrap();
        let p = db_with(d.path(), "t.db", FIXTURE).await;
        let r = query_read_only(&p, "SELECT name FROM sqlite_master WHERE type='table'", 10)
            .await
            .unwrap();
        assert!(
            r.rows.iter().any(|row| row[0] == Cell::Text("t".into())),
            "{:?}",
            r.rows
        );
    }

    /// A read-only open must not leave a migration table behind — nor any other
    /// trace. The file this reader is handed belongs to somebody else.
    #[tokio::test]
    async fn reading_leaves_no_migration_table_behind() {
        let d = tempfile::tempdir().unwrap();
        let p = db_with(d.path(), "t.db", FIXTURE).await;
        let _ = query_read_only(&p, "SELECT 1", 10).await.unwrap();
        let r = query_read_only(&p, "SELECT name FROM sqlite_master", 50)
            .await
            .unwrap();
        let names: Vec<&Cell> = r.rows.iter().map(|row| &row[0]).collect();
        assert!(
            !names.contains(&&Cell::Text("_sqlx_migrations".into())),
            "a reader that migrates has rewritten a file it does not own: {names:?}"
        );
    }
}
