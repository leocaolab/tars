"""B-20 W2 — Python validators end-to-end through Pipeline.complete.

These are integration tests: each one configures a `Pipeline` with
Python validators attached and dispatches a real `complete()` call to
the LM Studio provider running locally on `127.0.0.1:1234`.

Run:
    pip install pytest
    maturin develop --release
    pytest crates/tars-py/python/tests/test_validators.py

Skip when LM Studio isn't running (no failure):
    pytest -k "not requires_provider"   # if you tag tests
or just see them error out — failures are clearly "no provider".

Test goals (one per `def test_*`):

1.  Outcome classes are importable and field access works.
2.  No-op validator passes through; `validation` shows in layer trace.
3.  Reject → `TarsProviderError(kind='validation_failed')`.
4.  FilterText replaces `Response.text`.
5.  Validator chain: Filter result is visible to subsequent validator.
6.  Buggy validator (raises) — surfaces as permanent Reject, not crash.
7.  Wrong return type — surfaces as permanent Reject with type guidance.
8.  `validators=None` passes through unchanged (no validation layer).
"""

from __future__ import annotations

import pytest
import tars

# All tests target a Pipeline backed by `qwen_coder_local` from the
# user's `~/.tars/config.toml`. If that provider isn't reachable
# (LM Studio not running on :1234), every test fails with a clear
# network error — caller can decide to skip.
PROVIDER_ID = "qwen_coder_local"
MODEL = "qwen/qwen3-coder-30b"


def _pipeline_with(*validator_pairs):
    """Helper — build a Pipeline with the given (name, callable)
    validators attached."""
    return tars.Pipeline.from_default(
        PROVIDER_ID,
        validators=list(validator_pairs) if validator_pairs else None,
    )


# ── 1. Outcome classes are importable + constructible ────────────────


def test_outcome_classes_exposed():
    """All 4 outcome classes are exported under `tars.*`."""
    assert tars.Pass
    assert tars.Reject
    assert tars.FilterText
    assert tars.Annotate


def test_pass_construct():
    p = tars.Pass()
    assert "Pass()" in repr(p)


def test_reject_fields():
    r = tars.Reject("bad", retriable=True)
    assert r.reason == "bad"
    assert r.retriable is True


def test_filter_text_fields():
    f = tars.FilterText("clean", dropped=["bad"])
    assert f.text == "clean"
    assert f.dropped == ["bad"]


def test_filter_text_default_dropped_empty():
    f = tars.FilterText("clean")
    assert f.dropped == []


def test_annotate_metrics_dict():
    a = tars.Annotate({"score": 0.5, "n": 3})
    m = a.metrics
    assert m["score"] == 0.5
    assert m["n"] == 3


def test_annotate_default_metrics_empty():
    a = tars.Annotate()
    assert a.metrics == {}


# ── 2-8. Pipeline integration tests (require local provider) ────────


def test_validator_pass_through_appears_in_layer_trace():
    """Happy path: validator returns Pass → response succeeds, the
    'validation' layer is recorded in telemetry.layers."""
    calls: list[str] = []

    def always_pass(req, resp):
        calls.append(resp.get("text", "")[:30])
        return tars.Pass()

    p = _pipeline_with(("always_pass", always_pass))
    r = p.complete(model=MODEL, user="reply: ok", max_output_tokens=10)
    assert "validation" in r.telemetry.layers
    assert len(calls) == 1, f"validator should run exactly once, ran {len(calls)}"


def test_validator_reject_surfaces_validation_failed_error():
    """Reject(retriable=False) → TarsProviderError(kind='validation_failed',
    is_retriable=False). Validator name + reason in message."""

    def rejector(req, resp):
        return tars.Reject(reason="always reject", retriable=False)

    p = _pipeline_with(("always_reject", rejector))
    with pytest.raises(tars.TarsProviderError) as excinfo:
        p.complete(model=MODEL, user="hi", max_output_tokens=10)
    e = excinfo.value
    assert e.kind == "validation_failed"
    assert e.is_retriable is False
    assert "always_reject" in str(e)
    assert "always reject" in str(e)


def test_filter_text_replaces_response_text():
    """A FilterText validator on the chain transforms `Response.text`."""

    def yell(req, resp):
        return tars.FilterText(text=resp["text"].upper(), dropped=["lowercase"])

    p = _pipeline_with(("yell", yell))
    r = p.complete(model=MODEL, user="say hi briefly", max_output_tokens=20)
    assert r.text == r.text.upper(), f"expected upper-cased, got: {r.text!r}"


def test_chain_filter_visible_to_subsequent_validator():
    """Filter chains: validator B sees the post-Filter text from
    validator A. Asserts the chain ordering invariant."""
    seen_lengths: list[int] = []

    def truncate_to_5(req, resp):
        return tars.FilterText(text=resp["text"][:5], dropped=[])

    def length_check(req, resp):
        seen_lengths.append(len(resp["text"]))
        return tars.Pass()

    p = _pipeline_with(
        ("truncate", truncate_to_5),
        ("length_check", length_check),
    )
    r = p.complete(model=MODEL, user="say a long sentence", max_output_tokens=40)
    assert seen_lengths, "length_check validator never ran"
    assert seen_lengths[-1] <= 5, (
        f"length_check should see post-truncate text (≤5 chars); "
        f"got len={seen_lengths[-1]}"
    )
    assert len(r.text) <= 5


def test_buggy_validator_surfaces_as_permanent_reject_not_crash():
    """A validator that raises a Python exception is caught by the
    adapter and translated into ValidationFailed{retriable=false}.
    Asserts the worker isn't crashed by user-side bugs."""

    def buggy(req, resp):
        raise ValueError("bug in my validator")

    p = _pipeline_with(("buggy", buggy))
    with pytest.raises(tars.TarsProviderError) as excinfo:
        p.complete(model=MODEL, user="hi", max_output_tokens=10)
    e = excinfo.value
    assert e.kind == "validation_failed"
    assert e.is_retriable is False, (
        "validator-crash should be Permanent — re-running deterministically "
        "fails the same way"
    )
    assert "bug in my validator" in str(e)


def test_validator_wrong_return_type_surfaces_as_permanent_reject():
    """A validator that returns an unexpected type (e.g. plain str) is
    surfaced as a permanent Reject with guidance about valid outcomes."""

    def returns_string(req, resp):
        return "not an outcome"  # noqa — deliberately wrong

    p = _pipeline_with(("returns_string", returns_string))
    with pytest.raises(tars.TarsProviderError) as excinfo:
        p.complete(model=MODEL, user="hi", max_output_tokens=10)
    e = excinfo.value
    assert e.kind == "validation_failed"
    msg = str(e)
    # Adapter error message should at least mention the expected types.
    assert "tars.Pass" in msg or "validator" in msg


def test_validators_none_does_not_add_validation_layer():
    """Backward compat: `validators=None` (or omitted) → no
    `validation` layer in telemetry trace."""
    p = tars.Pipeline.from_default(PROVIDER_ID)  # validators kwarg omitted
    r = p.complete(model=MODEL, user="hi", max_output_tokens=10)
    assert "validation" not in r.telemetry.layers


def test_validators_empty_list_does_not_add_validation_layer():
    """`validators=[]` is equivalent to None (no validators registered
    means no ValidationMiddleware in the chain)."""
    p = tars.Pipeline.from_default(PROVIDER_ID, validators=[])
    r = p.complete(model=MODEL, user="hi", max_output_tokens=10)
    assert "validation" not in r.telemetry.layers


# ── Misuse / construction-error tests (no provider needed) ──────────


def test_validator_must_be_tuple_not_callable():
    """`validators=[my_callable]` (no name) is a TypeError, not a
    silent acceptance — caller's mistake should fail at construction
    not deep inside Pipeline.complete."""

    def my_callable(req, resp):
        return tars.Pass()

    with pytest.raises(TypeError) as excinfo:
        tars.Pipeline.from_default(PROVIDER_ID, validators=[my_callable])
    assert "tuple" in str(excinfo.value).lower()


def test_validator_tuple_must_be_pair():
    """A 1-element or 3-element tuple is rejected with a clear message."""
    with pytest.raises(ValueError) as excinfo:
        tars.Pipeline.from_default(
            PROVIDER_ID, validators=[("only_name",)]  # missing callable
        )
    assert "exactly" in str(excinfo.value).lower() or "2" in str(excinfo.value)
