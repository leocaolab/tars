-- Baseline for the LLM record store (raw request/response bodies, CAS-keyed).
-- `IF NOT EXISTS` keeps adoption of a pre-existing DB (an older rusqlite/refinery
-- `llm_records` table) a no-op — sqlx records this migration as applied in
-- `_sqlx_migrations` without re-creating the table.

CREATE TABLE IF NOT EXISTS llm_records (
    tenant_id    TEXT    NOT NULL,
    content_hash BLOB    NOT NULL,
    content      BLOB    NOT NULL,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, content_hash)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_llm_records_created_at
    ON llm_records(created_at);
