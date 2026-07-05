#!/usr/bin/env python3
"""A/B testing the *code-change* axis — pin the LLM, diff the code.

tars frames A/B on two axes (Doc 18 §5a):

  - LLM-change axis: the code is fixed, the prompt/model varies. Because
    the LLM is stochastic you diff *behavior* with statistics (McNemar /
    paired bootstrap) over many samples. See docs/eval-methodology.md.

  - code-change axis (THIS demo): the LLM is *pinned* (via a cassette) and
    the CODE varies — a refactor, a new parser, a threshold change. The
    diff is then EXACT and one sample suffices, because the only thing that
    moved is your code. "Did this change observable behavior?" — the
    regression question.

Here we reuse the *same* cassette the schema-validation demo recorded, so
the model's reply is held byte-for-byte constant, and A/B two versions of a
downstream transform over it. No live model — pure replay.

    python3 examples/python/ab-testing/code_change_ab.py

(Record the cassette first if it's missing:
 TARS_CASSETTE_RECORD=1 python3 examples/python/structured-output/schema_validation.py)
"""

from __future__ import annotations

import json
import os
import sys

import tars

CONFIG = os.environ.get("TARS_EXAMPLE_CONFIG", "examples/tars.toml")
PROVIDER = os.environ.get("TARS_EXAMPLE_PROVIDER", "cassette_schema")
MODEL = "qwen/qwen3-coder-30b"

# The EXACT request the schema-validation demo recorded — so it replays
# from the shared cassette (a different prompt would be a cassette MISS).
SYSTEM = "You output ONLY a JSON object. No prose, no code fence."
USER = (
    "Rate the severity (0-10 integer) of this bug and summarize it "
    "in one sentence. Return an object with keys `severity` and "
    "`summary`.\n\nBUG: unwrap() on a None in the request handler "
    "panics the whole worker on malformed input."
)


# ── The code under A/B: two versions of a "severity bucket" transform ──
# The refactor lowers the `critical` cutoff 9 → 8 (say, ops asked for a
# tighter paging policy). Everything else is unchanged.
def bucket_v_a(severity: int) -> str:
    if severity >= 9:
        return "critical"
    if severity >= 7:
        return "high"
    return "moderate"


def bucket_v_b(severity: int) -> str:
    if severity >= 8:  # critical cutoff moved 9 → 8
        return "critical"
    if severity >= 7:
        return "high"
    return "moderate"


def main() -> int:
    # Pin the LLM: one replayed completion, held constant across both arms.
    pipe = tars.Pipeline.from_config(CONFIG, PROVIDER)
    resp = pipe.complete(model=MODEL, system=SYSTEM, user=USER, max_output_tokens=200)
    review = json.loads(resp.text)
    severity = review["severity"]
    print(f"pinned model reply: severity={severity}  (telemetry wall={resp.telemetry})")

    # Run both code arms over the SAME pinned input → exact, deterministic diff.
    a = bucket_v_a(severity)
    b = bucket_v_b(severity)
    print("── code-change A/B (LLM pinned, one sample, exact diff) ──")
    print(f"  variant A (critical≥9): {a}")
    print(f"  variant B (critical≥8): {b}")
    if a == b:
        print("  → no behavior change on this input.")
    else:
        print(f"  → REGRESSION/CHANGE: {a!r} → {b!r} — the refactor moved observable output.")
    # On the real corpus you'd run this over N cases and let `tars eval diff`
    # tally the discordant cells (McNemar); here one pinned case shows the shape.
    return 0


if __name__ == "__main__":
    sys.exit(main())
