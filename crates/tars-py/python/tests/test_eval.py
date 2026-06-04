"""Doc 16 (re-scoped) — ``tars.eval`` read_calls / write_score helpers.

Drives real ``Pipeline.complete`` calls into an event store, then
exercises the evaluator loop: read finished calls, compute a score,
write it back as an ``EvaluationScored`` event. Requires LM Studio.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest
import tars
import tars.eval as ev

PROVIDER_ID = "qwen_coder_local"
MODEL = "qwen/qwen3-coder-30b"


def _seed_calls(store_dir: str, n: int = 2) -> None:
    """Run `n` real completions so the event store has calls to score."""
    p = tars.Pipeline.builder(PROVIDER_ID).event_store(store_dir).cache(False).build()
    for i in range(n):
        p.complete(model=MODEL, user=f"reply with the number {i}", max_output_tokens=5)
    import time

    time.sleep(0.3)  # let the fire-and-forget event writes flush


def test_read_calls_returns_finished_calls_as_dicts(tmp_path):
    _seed_calls(str(tmp_path), n=2)
    calls = ev.read_calls(str(tmp_path))
    assert len(calls) == 2
    c = calls[0]
    # Shape sanity: the fields an evaluator scores off.
    assert c["type"] == "llm_call_finished"
    assert c["actual_model"]
    assert "event_id" in c and "tenant_id" in c
    assert "usage" in c and "telemetry" in c
    assert c["result"] == {"result": "ok"}


def test_read_calls_limit_and_since(tmp_path):
    _seed_calls(str(tmp_path), n=3)
    assert len(ev.read_calls(str(tmp_path), limit=1)) == 1
    # A zero-width lookback window excludes everything.
    assert ev.read_calls(str(tmp_path), since_secs=0) == []


def test_write_score_appends_evaluation_scored_event(tmp_path):
    _seed_calls(str(tmp_path), n=1)
    call = ev.read_calls(str(tmp_path))[0]

    new_id = ev.write_score(
        str(tmp_path),
        call_event_id=call["event_id"],
        evaluator_name="len_score",
        score=0.75,
        tenant_id=call["tenant_id"],
        explanation="demo",
        tags=["dogfood"],
    )
    assert isinstance(new_id, str) and len(new_id) == 36  # UUID

    # Verify the score landed as an evaluation_scored row, FK'd to the call.
    db = Path(tmp_path) / "pipeline_events.db"
    with sqlite3.connect(db) as conn:
        rows = conn.execute(
            "SELECT payload_json FROM pipeline_events WHERE event_type = 'evaluation_scored'"
        ).fetchall()
    assert len(rows) == 1
    scored = json.loads(rows[0][0])
    assert scored["call_event_id"] == call["event_id"]
    assert scored["evaluator_name"] == "len_score"
    assert scored["score"] == 0.75
    assert scored["explanation"] == "demo"
    assert scored["tags"] == ["dogfood"]
    assert scored["event_id"] == new_id


def test_write_score_infers_tenant_when_omitted(tmp_path):
    _seed_calls(str(tmp_path), n=1)
    call = ev.read_calls(str(tmp_path))[0]
    # No tenant_id passed — it's looked up from the referenced call.
    ev.write_score(
        str(tmp_path),
        call_event_id=call["event_id"],
        evaluator_name="auto_tenant",
        score=1.0,
    )
    db = Path(tmp_path) / "pipeline_events.db"
    with sqlite3.connect(db) as conn:
        (payload,) = conn.execute(
            "SELECT payload_json FROM pipeline_events WHERE event_type='evaluation_scored'"
        ).fetchone()
    assert json.loads(payload)["tenant_id"] == call["tenant_id"]


def test_write_score_rejects_bad_uuid(tmp_path):
    _seed_calls(str(tmp_path), n=1)
    with pytest.raises(ValueError):
        ev.write_score(
            str(tmp_path),
            call_event_id="not-a-uuid",
            evaluator_name="x",
            score=0.0,
        )


def test_write_score_unknown_call_without_tenant_errors(tmp_path):
    _seed_calls(str(tmp_path), n=1)
    # Valid UUID shape, but no such call in the store, and no tenant given.
    with pytest.raises(ValueError):
        ev.write_score(
            str(tmp_path),
            call_event_id="00000000-0000-4000-8000-000000000000",
            evaluator_name="x",
            score=0.0,
        )


def test_read_calls_missing_store_raises(tmp_path):
    with pytest.raises(FileNotFoundError):
        ev.read_calls(str(tmp_path / "nope"))


def test_full_evaluator_loop_scores_every_call(tmp_path):
    """The canonical usage: read every call, score it, write it back —
    one evaluation_scored per llm_call_finished."""
    _seed_calls(str(tmp_path), n=3)
    calls = ev.read_calls(str(tmp_path))
    for call in calls:
        # Toy metric: 1.0 if the call produced any output tokens.
        produced = call["usage"]["output_tokens"] > 0
        ev.write_score(
            str(tmp_path),
            call_event_id=call["event_id"],
            evaluator_name="produced_output",
            score=1.0 if produced else 0.0,
            tenant_id=call["tenant_id"],
        )

    db = Path(tmp_path) / "pipeline_events.db"
    with sqlite3.connect(db) as conn:
        (n_scores,) = conn.execute(
            "SELECT COUNT(*) FROM pipeline_events WHERE event_type='evaluation_scored'"
        ).fetchone()
    assert n_scores == len(calls) == 3
