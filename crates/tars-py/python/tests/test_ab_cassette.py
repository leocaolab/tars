"""Cassette-backed regression test — the A/B code-change axis as a *test*,
not a CLI command.

The LLM is pinned by a committed cassette (examples/tars.toml →
`cassette_schema`), so this asserts on a real model's reply deterministically
with NO live provider. It is deliberately **not** marked `requires_provider`,
so it runs in CI (see conftest.py) — that's the whole point of the cassette.

Bless workflow when the pinned reply should change: re-record with
`TARS_CASSETTE_RECORD=1` (needs LM Studio once), commit the new cassette, and
the git diff of the .json is the review surface.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import tars

REPO_ROOT = Path(__file__).resolve().parents[4]

# The exact request the shared cassette recorded (must match byte-for-byte to
# replay — a change is a cassette MISS, which is the signal to re-record).
SYSTEM = "You output ONLY a JSON object. No prose, no code fence."
USER = (
    "Rate the severity (0-10 integer) of this bug and summarize it "
    "in one sentence. Return an object with keys `severity` and "
    "`summary`.\n\nBUG: unwrap() on a None in the request handler "
    "panics the whole worker on malformed input."
)


def _complete_pinned():
    # Cassette paths in tars.toml are cwd-relative → run from repo root.
    os.chdir(REPO_ROOT)
    pipe = tars.Pipeline.from_config("examples/tars.toml", "cassette_schema")
    return pipe.complete(
        model="qwen/qwen3-coder-30b", system=SYSTEM, user=USER, max_output_tokens=200
    )


def test_pinned_reply_is_schema_valid():
    resp = _complete_pinned()
    data = json.loads(resp.text)  # replayed reply parses as JSON
    assert isinstance(data["severity"], int)
    assert isinstance(data["summary"], str)


def test_pinned_reply_is_deterministic():
    # Same cassette entry twice → byte-identical (replay is a pure function).
    a = _complete_pinned().text
    b = _complete_pinned().text
    assert a == b


def test_severity_bucket_snapshot():
    # Snapshot of a downstream transform over the pinned reply. When a refactor
    # changes this, the assert fails → you bless by updating the expected value.
    severity = json.loads(_complete_pinned().text)["severity"]

    def bucket(s: int) -> str:
        return "critical" if s >= 9 else "high" if s >= 7 else "moderate"

    assert bucket(severity) == "high"  # pinned severity is 8
