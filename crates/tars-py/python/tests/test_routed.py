"""B-8 landing — `Pipeline.routed(...)` multi-provider routing.

Construction tests need only `~/.tars/config.toml` (they build the
routing onion but make no call). The `requires_provider` ones drive real
completions to prove the observe→route loop (LatencyPolicy reorders,
RoutingService records dispatch latency) works end to end through the
wheel, not just in Rust mock tests.
"""

from __future__ import annotations

import pytest
import tars

PROVIDER_ID = "qwen_coder_local"
MODEL = "qwen/qwen3-coder-30b"


def test_routed_builds_latency_pipeline_layers():
    p = tars.Pipeline.routed([PROVIDER_ID], policy="latency")
    assert p.id == f"routed[{PROVIDER_ID}]"
    # cache defaults off for routed; no validators/events here.
    assert p.layer_names == ["telemetry", "retry"]


def test_routed_static_policy_builds():
    p = tars.Pipeline.routed([PROVIDER_ID], policy="static")
    assert "telemetry" in p.layer_names


def test_routed_cache_opt_in_adds_cache_layer():
    p = tars.Pipeline.routed([PROVIDER_ID], policy="static", cache=True)
    assert "cache_lookup" in p.layer_names


def test_routed_latency_stats_empty_before_any_call():
    p = tars.Pipeline.routed([PROVIDER_ID], policy="latency")
    assert p.latency_stats() == {}


def test_routed_static_has_no_latency_stats():
    p = tars.Pipeline.routed([PROVIDER_ID], policy="static")
    assert p.latency_stats() == {}


def test_routed_empty_provider_list_errors():
    with pytest.raises(ValueError):
        tars.Pipeline.routed([], policy="latency")


def test_routed_unknown_provider_errors():
    with pytest.raises(tars.TarsConfigError):
        tars.Pipeline.routed(["no_such_provider_xyz"], policy="latency")


def test_routed_unknown_policy_errors():
    with pytest.raises(ValueError):
        tars.Pipeline.routed([PROVIDER_ID], policy="round_robin")


def test_routed_bad_latency_metric_errors():
    with pytest.raises(ValueError):
        tars.Pipeline.routed([PROVIDER_ID], policy="latency", latency_metric="p42")


def test_routed_config_path_and_str_mutually_exclusive():
    with pytest.raises(ValueError):
        tars.Pipeline.routed(
            [PROVIDER_ID], config_path="/x.toml", config_str="x = 1"
        )


# ── Live (requires LM Studio) ────────────────────────────────────────


@pytest.mark.requires_provider
def test_routed_latency_pipeline_records_dispatch_latency():
    """Real calls through a routed latency pipeline feed per-provider
    latency, readable via latency_stats(). Proves the observe→route loop
    is wired into the live path (not just Rust mock tests)."""
    p = tars.Pipeline.routed([PROVIDER_ID], policy="latency")
    for _ in range(3):
        r = p.complete(model=MODEL, user="reply: ok", max_output_tokens=5)
        assert r.text  # the call actually reached the backend

    stats = p.latency_stats()
    assert PROVIDER_ID in stats
    s = stats[PROVIDER_ID]
    assert s["count"] == 3
    # All summary fields present and non-negative.
    for k in ("mean_ms", "p50_ms", "p95_ms"):
        assert s[k] >= 0


@pytest.mark.requires_provider
def test_routed_static_pipeline_serves_without_stats():
    p = tars.Pipeline.routed([PROVIDER_ID], policy="static")
    r = p.complete(model=MODEL, user="reply: ok", max_output_tokens=5)
    assert r.text
    assert p.latency_stats() == {}  # static never records
