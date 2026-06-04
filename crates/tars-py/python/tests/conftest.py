"""Shared pytest fixtures / collection hooks for the tars-py suite.

`requires_provider`-marked tests need a live LLM backend on
127.0.0.1:1234 (LM Studio / qwen_coder_local). Rather than hand-maintain
a CI `-k` allowlist, we auto-skip those tests when the port is
unreachable — so:

- CI (no provider) runs every construction/config test and skips only
  the live ones, with a clear "no provider" skip reason.
- Locally (provider up) the full suite runs, including the live tests.

Tests that only need `~/.tars/config.toml` (e.g. builder layer-name
assertions — they build a provider object but never call it) are NOT
marked and run everywhere a config file exists.
"""

from __future__ import annotations

import socket

import pytest


def _provider_reachable(host: str = "127.0.0.1", port: int = 1234, timeout: float = 0.3) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


_PROVIDER_UP = _provider_reachable()


def pytest_collection_modifyitems(config, items):
    if _PROVIDER_UP:
        return
    skip = pytest.mark.skip(reason="no LLM provider reachable on 127.0.0.1:1234")
    for item in items:
        if "requires_provider" in item.keywords:
            item.add_marker(skip)
