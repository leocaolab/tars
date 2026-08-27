-- Baseline for the AgentEventLog (append-only trajectory events).
-- A live DB created by the old inline DDL adopts cleanly — the tables already
-- exist, so this `IF NOT EXISTS` baseline is a no-op and sqlx records the
-- migration as applied in `_sqlx_migrations` (the version-of-record). See CUJ-3.

CREATE TABLE IF NOT EXISTS events (
    trajectory_id   TEXT    NOT NULL,
    sequence_no     INTEGER NOT NULL,
    timestamp_ms    INTEGER NOT NULL,
    payload_json    BLOB    NOT NULL,
    PRIMARY KEY (trajectory_id, sequence_no)
) STRICT;

-- The PK already covers (trajectory_id, sequence_no) lookups; a separate index
-- for "list trajectories" is cheaper than a DISTINCT scan over the PK.
CREATE INDEX IF NOT EXISTS idx_events_trajectory
    ON events(trajectory_id);
