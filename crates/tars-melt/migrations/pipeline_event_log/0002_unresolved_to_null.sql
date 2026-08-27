-- v1 -> v2 data rewrite (ARC-L5-SW-10).
--
-- `LlmCallFinished.provider_id` changed from `ProviderId` (with a
-- `"unresolved"` sentinel string for the "no provider ran" case) to
-- `Option<ProviderId>`. Rewrite any `llm_call_finished` payload carrying
-- the legacy `provider_id: "unresolved"` sentinel into
-- `provider_id: null` so it matches the new shape.
--
-- This reproduces the old `migrate_v1_to_v2_unresolved_to_null` Rust
-- pass exactly:
--   * Scope to `event_type = 'llm_call_finished'` — the only variant
--     that carries `provider_id`.
--   * Only rows whose `$.LlmCallFinished.provider_id` is the string
--     `"unresolved"` are touched; every other row (already-null,
--     already-resolved, no `LlmCallFinished` body, or absent field) is
--     left byte-for-byte unchanged, so the WHERE-matched set and the
--     row count are identical to the Rust version.
--   * `json_valid(...)` guard skips a malformed payload rather than
--     failing the migration — the Rust pass logged+skipped unparseable
--     rows so the operator could still open the DB. Preserved (the row
--     is left as-is); the per-row warning is dropped (a SQL migration
--     has nowhere to log), which is cosmetic.
--
-- Mechanics: `payload_json` is a STRICT BLOB holding UTF-8 JSON text, so
-- `CAST(... AS TEXT)` is needed to feed SQLite's JSON functions text
-- (rather than have a BLOB argument interpreted as JSONB), and the
-- rewritten value is `CAST(... AS BLOB)` before storing back into the
-- STRICT BLOB column. `json_set(text, path, NULL)` maps the SQL NULL to
-- a JSON null. Idempotent — a second run finds no `"unresolved"` rows.

UPDATE pipeline_events
SET payload_json = CAST(
        json_set(
            CAST(payload_json AS TEXT),
            '$.LlmCallFinished.provider_id',
            NULL
        ) AS BLOB
    )
WHERE event_type = 'llm_call_finished'
  AND json_valid(CAST(payload_json AS TEXT))
  AND json_extract(CAST(payload_json AS TEXT), '$.LlmCallFinished.provider_id') = 'unresolved';
