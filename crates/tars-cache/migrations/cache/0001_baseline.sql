-- Baseline for the Personal-mode L2 response cache (SQLite).
-- `IF NOT EXISTS` keeps adoption of a pre-existing DB (an older rusqlite
-- `cache_entries` table written under `PRAGMA user_version = 1`) a no-op —
-- sqlx records this migration as applied in `_sqlx_migrations` without
-- re-creating the table.

CREATE TABLE IF NOT EXISTS cache_entries (
    fingerprint   BLOB    PRIMARY KEY,
    value         BLOB    NOT NULL,
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_cache_expires
    ON cache_entries(expires_at_ms);
