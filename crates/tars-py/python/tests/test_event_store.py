"""B-20.W3 enabler — Pipeline.with_event_store(...) integration test.

Drives a real `Pipeline.complete` and asserts:

  1. After the call, `pipeline_events.db` exists under `event_store_dir`.
  2. It contains exactly one `LlmCallFinished` row.
  3. `bodies.db` has fetchable request + response body bytes referenced
     by the event row.
  4. validation_summary on the event matches what Response surfaces.

We use sqlite3 module directly to peek at the on-disk schema rather
than going through a Python-side `EventStore` API (which is intentionally
not exposed in Phase 1 — query API lands with W3 main body).
"""

import json
import sqlite3
from pathlib import Path

import pytest
import tars

PROVIDER_ID = "qwen_coder_local"
MODEL = "qwen/qwen3-coder-30b"


def test_event_lands_in_pipeline_events_db(tmp_path):
    p = tars.Pipeline.from_default(PROVIDER_ID, event_store_dir=str(tmp_path))
    r = p.complete(model=MODEL, user="reply: ok", max_output_tokens=10)

    # Caller still gets a normal Response — emit is fire-and-forget,
    # not blocking.
    assert r.text  # non-empty response from local LLM

    # Allow background spawn to flush.
    import time
    time.sleep(0.2)

    db = tmp_path / "pipeline_events.db"
    assert db.exists(), f"event store db should exist at {db}"

    with sqlite3.connect(db) as conn:
        rows = conn.execute(
            "SELECT event_type, tenant_id, payload_json FROM pipeline_events"
        ).fetchall()
    assert len(rows) == 1, f"expected 1 event row, got {len(rows)}"
    event_type, tenant_id, payload_blob = rows[0]
    assert event_type == "llm_call_finished"
    assert tenant_id  # non-empty (test_default uses 'tenant-test')

    payload = json.loads(payload_blob)
    assert payload["type"] == "llm_call_finished"
    assert payload["actual_model"]
    assert payload["result"] == {"result": "ok"}


def test_request_and_response_bodies_are_fetchable(tmp_path):
    p = tars.Pipeline.from_default(PROVIDER_ID, event_store_dir=str(tmp_path))
    p.complete(model=MODEL, user="hi", max_output_tokens=10)

    import time
    time.sleep(0.2)

    db_events = tmp_path / "pipeline_events.db"
    db_bodies = tmp_path / "bodies.db"
    assert db_events.exists() and db_bodies.exists()

    # Pull the event row to get the ContentRef hash bytes.
    with sqlite3.connect(db_events) as conn:
        (payload_blob,) = conn.execute(
            "SELECT payload_json FROM pipeline_events LIMIT 1"
        ).fetchone()
    payload = json.loads(payload_blob)
    req_ref = payload["request_ref"]
    resp_ref = payload["response_ref"]

    # ContentRef serialized form: { tenant_id, body_hash: [u8; 32] }
    assert req_ref["tenant_id"]
    assert resp_ref is not None, "response_ref present on Ok call"

    # Bodies table — fetch by (tenant_id, body_hash).
    with sqlite3.connect(db_bodies) as conn:
        rows = conn.execute(
            "SELECT tenant_id, length(body) FROM bodies"
        ).fetchall()
    assert len(rows) == 2, f"expected 2 body rows (req + resp), got {len(rows)}"
    for tenant_id, body_len in rows:
        assert tenant_id  # non-empty
        assert body_len > 0  # bodies are non-empty serialized JSON


def test_validation_summary_propagates_to_event(tmp_path):
    """Validators run, dropped lists hit Response.validation_summary
    AND the persisted event row's validation_summary."""
    def truncate(req, resp):
        return tars.FilterText(text=resp["text"][:5], dropped=["over5"])

    p = tars.Pipeline.from_default(
        PROVIDER_ID,
        event_store_dir=str(tmp_path),
        validators=[("trunc", truncate)],
    )
    r = p.complete(model=MODEL, user="say a long sentence", max_output_tokens=40)
    assert r.validation_summary.outcomes["trunc"]["dropped"] == ["over5"]

    import time
    time.sleep(0.2)

    db = tmp_path / "pipeline_events.db"
    with sqlite3.connect(db) as conn:
        (payload_blob,) = conn.execute(
            "SELECT payload_json FROM pipeline_events LIMIT 1"
        ).fetchone()
    payload = json.loads(payload_blob)
    summary = payload["validation_summary"]
    assert summary["validators_run"] == ["trunc"]
    assert summary["outcomes"]["trunc"]["dropped"] == ["over5"]


def test_event_store_dir_omitted_does_not_create_files(tmp_path):
    """Backwards compat: omitting event_store_dir = no event store wiring,
    no files created, EventEmitter not in layer trace."""
    p = tars.Pipeline.from_default(PROVIDER_ID)
    r = p.complete(model=MODEL, user="hi", max_output_tokens=10)
    # No event_emitter layer.
    assert "event_emitter" not in r.telemetry.layers
    # tmp_path was never wired — should be empty.
    assert not list(tmp_path.iterdir())


def test_tags_propagate_to_event(tmp_path):
    """Cohort tags passed via Pipeline.complete(tags=[...]) land on
    the persisted event row. Enables `WHERE 'X' IN tags` SQL rollups."""
    p = tars.Pipeline.from_default(PROVIDER_ID, event_store_dir=str(tmp_path))
    p.complete(
        model=MODEL,
        user="hi",
        max_output_tokens=10,
        tags=["dogfood_2026_05_08", "tier_1_validators"],
    )

    import time
    time.sleep(0.2)

    db = tmp_path / "pipeline_events.db"
    with sqlite3.connect(db) as conn:
        (payload_blob,) = conn.execute(
            "SELECT payload_json FROM pipeline_events LIMIT 1"
        ).fetchone()
    payload = json.loads(payload_blob)
    assert payload["tags"] == ["dogfood_2026_05_08", "tier_1_validators"]


def test_tags_default_empty(tmp_path):
    """Omitting tags = empty list on the event, not missing key."""
    p = tars.Pipeline.from_default(PROVIDER_ID, event_store_dir=str(tmp_path))
    p.complete(model=MODEL, user="hi", max_output_tokens=5)

    import time
    time.sleep(0.2)

    db = tmp_path / "pipeline_events.db"
    with sqlite3.connect(db) as conn:
        (payload_blob,) = conn.execute(
            "SELECT payload_json FROM pipeline_events LIMIT 1"
        ).fetchone()
    payload = json.loads(payload_blob)
    assert payload["tags"] == []


def test_event_store_dir_creates_dir_if_missing(tmp_path):
    """Caller may pass a path that doesn't exist yet — middleware
    creates the directory, doesn't fail at construction time."""
    nested = tmp_path / "deeply" / "nested" / "dir"
    assert not nested.exists()
    p = tars.Pipeline.from_default(PROVIDER_ID, event_store_dir=str(nested))
    p.complete(model=MODEL, user="hi", max_output_tokens=5)
    assert nested.exists()
    assert (nested / "pipeline_events.db").exists()
