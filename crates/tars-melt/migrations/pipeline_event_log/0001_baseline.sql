-- Baseline for the pipeline event store (one row per `Pipeline.call`
-- boundary). This is the v1 schema that the hand-rolled `PRAGMA
-- user_version` migrator created inline.
--
-- `IF NOT EXISTS` keeps adoption of a pre-existing DB (an older
-- rusqlite `pipeline_events` table at user_version 1 or 2) a no-op —
-- sqlx records this migration as applied in `_sqlx_migrations` without
-- re-creating the table.
--
-- Schema notes:
-- - `event_id` is TEXT for UUID readability; PRIMARY KEY makes
--   re-append idempotent (INSERT OR REPLACE).
-- - Inline columns are pulled out of payload_json so cohort queries
--   (WHERE tenant + time range) don't have to parse JSON per row.
-- - The full `payload_json` BLOB is the source of truth; inline columns
--   are derived for query speed.

CREATE TABLE IF NOT EXISTS pipeline_events (
    event_id        TEXT    NOT NULL PRIMARY KEY,
    event_type      TEXT    NOT NULL,
    timestamp_ms    INTEGER NOT NULL,
    tenant_id       TEXT    NOT NULL,
    payload_json    BLOB    NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_pe_tenant_ts
    ON pipeline_events(tenant_id, timestamp_ms);
CREATE INDEX IF NOT EXISTS idx_pe_ts
    ON pipeline_events(timestamp_ms);
