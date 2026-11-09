-- The durable store's schema. Every table is STRICT, so a value of the wrong type is
-- refused at the write instead of stored under SQLite's type affinity.

-- The queue: one row per task, with its lease, park, interruption streak and result.
CREATE TABLE tasks (
    id                TEXT PRIMARY KEY,
    job_id            TEXT NOT NULL,
    agent_key         TEXT NOT NULL,
    intent            TEXT NOT NULL,
    -- The agent key of the task's judge, or NULL when the acting agent judges itself.
    judge             TEXT,
    status            TEXT NOT NULL,
    lease_owner       TEXT,
    lease_expires     INTEGER,
    -- Which claim the lease belongs to, minted by every claim and cleared with the owner
    -- and expiry: the owner alone cannot tell two claims of one task apart.
    lease_token       TEXT,
    attempt           INTEGER NOT NULL DEFAULT 0,
    continuation      TEXT,
    -- What a parked task waits for, as JSON (`Wait`); NULL while it is not parked.
    wait              TEXT,
    -- A parked task's own deadline (epoch millis), or NULL to wait indefinitely: the
    -- deadline `wait` carries, as a column the sweep can index. It lives
    -- on the row because a parked task holds no lease and runs no code that could watch
    -- a clock; the store sweeps for it as it sweeps for expired leases.
    wake_at           INTEGER,
    wake_pending      INTEGER NOT NULL DEFAULT 0,
    reply             TEXT,
    result            TEXT,
    -- How many resources the task has taken and not given back. The journal is the
    -- truth; this indexes it, written in the same transaction as the delta it counts, so
    -- "which terminal tasks still hold something" is one indexed query. A release never
    -- takes it below zero.
    holds             INTEGER NOT NULL DEFAULT 0 CHECK (holds >= 0),
    -- Where the task was last interrupted (its count of `delta` events) and how many
    -- times in a row it was interrupted there. Both NULL means never; they are written
    -- together, by the reclaim that puts the task back to `ready`.
    interrupted_at    INTEGER,
    interrupted_times INTEGER
) STRICT;

-- Claim is `WHERE status='ready' LIMIT 1` and reclaim `WHERE status='running'`: both
-- must be index lookups, never a scan.
CREATE INDEX idx_tasks_status ON tasks (status);
-- Partial: the sweep only asks for rows with a deadline.
CREATE INDEX idx_tasks_wake_at ON tasks (wake_at) WHERE wake_at IS NOT NULL;
-- Partial: the sweep only asks for rows that owe something, and in a healthy store that
-- set is empty.
CREATE INDEX idx_tasks_holds ON tasks (holds) WHERE holds > 0;
-- "No live child of T" asks for the tasks whose continuation is T.
CREATE INDEX idx_tasks_continuation ON tasks (continuation) WHERE continuation IS NOT NULL;

-- The append-only event log. `seq` is handed out by AUTOINCREMENT and is a reader's
-- cursor, so a row at or below zero would be passed over by every `since`; the CHECK
-- refuses one at the write.
CREATE TABLE trajectory (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT CHECK (seq > 0),
    task_id TEXT NOT NULL,
    kind    TEXT NOT NULL,
    at      INTEGER NOT NULL
) STRICT;

-- The reclaim counts one task's journal entries, and the per-task read selects them.
CREATE INDEX idx_trajectory_task ON trajectory (task_id);

-- Idempotency ledger for exactly-once external effects: a resumed step that re-derives
-- the same key gets the recorded result back instead of firing the write again. The
-- first write wins, because it is what the external system did.
CREATE TABLE effect_ledger (
    key    TEXT PRIMARY KEY,
    result TEXT NOT NULL
) STRICT;

-- Sealed journals. A terminal task's `trajectory` rows move into a Parquet file; these
-- two tables say which file is visible and where each task landed.
--
-- A file is registered `staged` before it is written, so a crash can leave only a named,
-- invisible file. It turns `live` in the same transaction that writes the task's
-- `journal_index` row and deletes its `trajectory` rows: one commit point, so a journal
-- row is always in exactly one place.
CREATE TABLE journal_file (
    name       TEXT PRIMARY KEY,
    state      TEXT NOT NULL CHECK (state IN ('staged', 'live')),
    created_at INTEGER NOT NULL
) STRICT;

-- A row here means the task is sealed; appending to its journal is refused. The seqs are
-- the first and last of the task's `trajectory` rows, so they obey that table's CHECK and
-- come in order.
CREATE TABLE journal_index (
    task_id   TEXT PRIMARY KEY,
    file      TEXT NOT NULL REFERENCES journal_file (name),
    row_group INTEGER NOT NULL CHECK (row_group >= 0),
    first_seq INTEGER NOT NULL CHECK (first_seq > 0),
    last_seq  INTEGER NOT NULL,
    CHECK (first_seq <= last_seq)
) STRICT;

-- Facts about the store itself, in at most one row. `tenant` is whose work it holds and
-- `store_id` its id: the runner that first opens the store writes the row, every later
-- open compares the tenant, and a reader with no tenant of its own takes both from here.
-- No row means no runner has claimed the store.
CREATE TABLE store_meta (
    one      INTEGER PRIMARY KEY CHECK (one = 1),
    tenant   TEXT NOT NULL CHECK (tenant <> ''),
    store_id TEXT NOT NULL CHECK (store_id <> '')
) STRICT;
