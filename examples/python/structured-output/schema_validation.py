#!/usr/bin/env python3
"""Getting a schema-validated result out of tars from Python.

Two complementary mechanisms — the same two the USER-GUIDE documents:

  1. `response_schema=`  — decode-time enforcement. The JSON Schema is
     handed to the provider's structured-output mode; a strict-capable
     provider *forces* conforming JSON, so `resp.text` is clean by
     construction and a plain `json.loads` is enough.

  2. an output validator — post-hoc, defense-in-depth. A Python callable
     attached via `validators=` runs inside the pipeline and can `Reject`
     a malformed reply (which surfaces as a typed `TarsProviderError`).

By default it talks to a **cassette** provider (record-once / replay-forever),
so it runs deterministically WITHOUT a live model — see `examples/tars.toml`.
No external Python deps — schema check is plain Python. Swap in `jsonschema` /
pydantic for a richer check.

    # replay from the committed cassette (no live model):
    python3 examples/python/structured-output/schema_validation.py

    # (re)record against live LM Studio, then it replays forever:
    TARS_CASSETTE_RECORD=1 python3 examples/python/structured-output/schema_validation.py

Override provider/config with TARS_EXAMPLE_PROVIDER / TARS_EXAMPLE_CONFIG
(e.g. TARS_EXAMPLE_PROVIDER=qwen_coder_local to hit LM Studio directly).
Run from the repo root — the cassette path in tars.toml is cwd-relative.
"""

from __future__ import annotations

import json
import os
import sys

import tars

CONFIG = os.environ.get("TARS_EXAMPLE_CONFIG", "examples/tars.toml")
PROVIDER = os.environ.get("TARS_EXAMPLE_PROVIDER", "cassette_schema")
MODEL = "qwen/qwen3-coder-30b"

# The shape we want back. Kept as data so both mechanisms can use it.
SCHEMA = {
    "type": "object",
    "properties": {
        "severity": {"type": "integer"},
        "summary": {"type": "string"},
    },
    "required": ["severity", "summary"],
}


def check_shape(data: object) -> str | None:
    """Plain-Python structural check. Returns an error string, or None if
    the value satisfies SCHEMA. (Swap for `jsonschema.validate` /
    `pydantic.BaseModel.model_validate` when you want a real validator.)"""
    if not isinstance(data, dict):
        return f"expected a dict, got {type(data).__name__}"
    for field in SCHEMA["required"]:
        if field not in data:
            return f"missing required field {field!r}"
    if not isinstance(data.get("severity"), int):
        return "severity must be an int"
    if not isinstance(data.get("summary"), str):
        return "summary must be a str"
    return None


def validate_schema(req, resp):
    """Output validator — parse + shape-check the model reply. On failure
    carry the *raw* reason out (not a sentinel), so the caller sees what
    actually went wrong.

    Inside a validator, `req` / `resp` are **dict views** (`resp["text"]`),
    not the `Response` object `complete()` returns."""
    text = resp["text"]
    try:
        data = json.loads(text)
    except json.JSONDecodeError as e:
        return tars.Reject(reason=f"not JSON: {e}; raw={text[:120]!r}")
    err = check_shape(data)
    if err is not None:
        return tars.Reject(reason=f"{err}; raw={text[:120]!r}")
    return tars.Pass()


def main() -> int:
    # ── Mechanism 1: decode-time enforcement via response_schema ──────
    # The validator is also attached, so both run: the provider shapes
    # the output, and the validator is the belt-and-suspenders check.
    pipe = tars.Pipeline.from_config(
        CONFIG, PROVIDER, validators=[("schema", validate_schema)]
    )

    # NOTE on mechanism 1 (`response_schema=`): decode-time enforcement
    # needs a strict-capable provider (Anthropic / OpenAI / Gemini). A local
    # LM Studio model may not accept `response_format`, so this local demo
    # relies on a JSON-forcing system prompt + the validator (mechanism 2).
    # Against a cloud provider, add `response_schema=SCHEMA` here.
    resp = pipe.complete(
        model=MODEL,
        system="You output ONLY a JSON object. No prose, no code fence.",
        user=(
            "Rate the severity (0-10 integer) of this bug and summarize it "
            "in one sentence. Return an object with keys `severity` and "
            "`summary`.\n\nBUG: unwrap() on a None in the request handler "
            "panics the whole worker on malformed input."
        ),
        max_output_tokens=200,
    )

    print("── raw resp.text ──")
    print(resp.text)
    print("── validator outcomes ──")
    print(resp.validation_summary)

    # resp.text passed the validator → safe to parse into a local value.
    review = json.loads(resp.text)
    print("── strong-typed-ish local value ──")
    print(f"severity={review['severity']!r}  summary={review['summary']!r}")

    # ── Mechanism 2: prove the Reject path is typed, not a crash ──────
    # Feed the same validator a deliberately-wrong shape via a validator
    # that always sees bad text — here we just call it directly to show
    # the outcome type without burning another model call.
    bad_resp = {"text": '{"severity": "high"}'}  # severity should be int → reject
    outcome = validate_schema(None, bad_resp)
    print("── direct validator call on bad shape ──")
    print(f"outcome type: {type(outcome).__name__}, reason: {outcome.reason}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
