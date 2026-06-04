"""B-6c — `Pipeline.builder()` fluent middleware builder.

Most tests are construction-only: `build()` resolves the provider from
`~/.tars/config.toml` and assembles the onion, but does NOT call the
backend, so `.layer_names` is assertable without a live LLM. The one
live test (cache on/off behaviour) is marked and needs LM Studio.
"""

from __future__ import annotations

import pytest
import tars

PROVIDER_ID = "qwen_coder_local"
MODEL = "qwen/qwen3-coder-30b"


def test_builder_exported_and_constructs():
    b = tars.Pipeline.builder(PROVIDER_ID)
    assert isinstance(b, tars.PipelineBuilder)
    assert "qwen_coder_local" in repr(b)


def test_builder_default_matches_from_default_layers():
    """A bare builder produces the same canonical onion as from_default."""
    built = tars.Pipeline.builder(PROVIDER_ID).build()
    default = tars.Pipeline.from_default(PROVIDER_ID)
    assert built.layer_names == default.layer_names
    assert built.layer_names == ["telemetry", "cache_lookup", "retry"]


def test_builder_is_chainable():
    """Each config method returns the builder for chaining."""
    b = tars.Pipeline.builder(PROVIDER_ID)
    assert b.retry(max_attempts=5) is b
    assert b.cache(False) is b
    assert b.event_store("/tmp/does-not-matter-not-built") is b


def test_builder_cache_false_drops_cache_layer():
    with_cache = tars.Pipeline.builder(PROVIDER_ID).build()
    no_cache = tars.Pipeline.builder(PROVIDER_ID).cache(False).build()
    assert "cache_lookup" in with_cache.layer_names
    assert "cache_lookup" not in no_cache.layer_names
    assert no_cache.layer_names == ["telemetry", "retry"]


def test_builder_retry_override_still_builds_retry_layer():
    p = (
        tars.Pipeline.builder(PROVIDER_ID)
        .retry(max_attempts=5, initial_backoff_ms=100, max_backoff_ms=10_000, multiplier=2.0)
        .build()
    )
    # Retry config is internal; what we can assert from Python is that the
    # layer is still present and the build succeeded.
    assert "retry" in p.layer_names


def test_builder_retry_disable_via_max_attempts_one():
    # max_attempts=1 disables retry but the layer is still wired (it just
    # never retries) — assert it builds cleanly.
    p = tars.Pipeline.builder(PROVIDER_ID).retry(max_attempts=1).build()
    assert "retry" in p.layer_names


def test_builder_validators_add_validation_layer():
    def always_pass(req, resp):
        return tars.Pass()

    p = tars.Pipeline.builder(PROVIDER_ID).validators([("v", always_pass)]).build()
    # Validation sits between telemetry and cache in the canonical onion.
    assert p.layer_names == ["telemetry", "validation", "cache_lookup", "retry"]


def test_builder_event_store_adds_event_emitter(tmp_path):
    p = tars.Pipeline.builder(PROVIDER_ID).event_store(str(tmp_path)).build()
    assert p.layer_names[0] == "event_emitter"


def test_builder_config_path_and_str_mutually_exclusive():
    with pytest.raises(ValueError):
        tars.Pipeline.builder(PROVIDER_ID, config_path="/x.toml", config_str="x = 1")


def test_builder_build_is_repeatable():
    """build() takes &self, so it can be called more than once."""
    b = tars.Pipeline.builder(PROVIDER_ID).cache(False)
    p1 = b.build()
    p2 = b.build()
    assert p1.layer_names == p2.layer_names == ["telemetry", "retry"]


def test_builder_from_inline_config_str():
    """config_str routes provider resolution through inline TOML, without
    touching ~/.tars/config.toml. We only assert the onion assembles
    (the base_url is a dead port — no call is made)."""
    toml = """
[providers.echo]
type = "openai_compat"
default_model = "m"
base_url = "http://127.0.0.1:9/v1"
"""
    p = tars.Pipeline.builder("echo", config_str=toml).cache(False).build()
    assert p.layer_names == ["telemetry", "retry"]


# ── Live test (requires LM Studio on :1234) ──────────────────────────


def test_builder_cache_toggle_reflected_in_runtime_layer_trace():
    """cache(False) removes the cache layer, so a real call's runtime
    telemetry.layers trace omits `cache_lookup` (and the default keeps
    it). This is the observable runtime effect of the toggle, end to end
    through a live provider call."""
    prompt = "reply with exactly: ok"

    cached = tars.Pipeline.builder(PROVIDER_ID).build()
    r = cached.complete(model=MODEL, user=prompt, max_output_tokens=5)
    assert "cache_lookup" in r.telemetry.layers

    uncached = tars.Pipeline.builder(PROVIDER_ID).cache(False).build()
    u = uncached.complete(model=MODEL, user=prompt, max_output_tokens=5)
    assert "cache_lookup" not in u.telemetry.layers
    assert "provider" in u.telemetry.layers  # still reaches the backend
