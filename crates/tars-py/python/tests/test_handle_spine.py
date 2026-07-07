"""Doc 12 §6 — the config + runtime-handle spine.

Exercises the whole spine WITHOUT a live backend: ``init`` (global config) →
``Workspaces.open`` → ``handle.pipeline / provider / role_provider`` → a
``with handle.context(...)`` block → ``close``. Construction resolves and wraps
providers but never calls one (the ``echo`` provider points at a dead port), so
this runs everywhere — no ``requires_provider`` mark.

``init`` / ``ProviderRegistry::global`` are process-global ``OnceLock``s
(first-load wins). The existing suite never calls ``init`` (it uses
``Pipeline.from_*`` which builds its own registry), so this test is the one
that establishes the global config for the process — hence a module-scoped
``home`` whose providers cover the role this test resolves.
"""

from __future__ import annotations

import os
import textwrap
from pathlib import Path

import pytest
import tars

# openai_compat builds fine against a dead base_url — no call is made here.
# Two providers, deliberately: with a single-provider registry the handle's
# role resolution falls back to the sole provider for ANY name (rule 5), which
# would mask the unknown-role path we assert below.
_HOME_CONFIG = textwrap.dedent(
    """
    [providers.echo]
    type = "openai_compat"
    default_model = "m"
    base_url = "http://127.0.0.1:9/v1"

    [providers.echo2]
    type = "openai_compat"
    default_model = "m"
    base_url = "http://127.0.0.1:9/v1"
    """
)

_WORKSPACE_CONFIG = textwrap.dedent(
    """
    [roles]
    critic = "echo"
    """
)


@pytest.fixture(scope="module")
def tars_home(tmp_path_factory) -> Path:
    home = tmp_path_factory.mktemp("tars_home")
    (home / "config.toml").write_text(_HOME_CONFIG)
    # Idempotent + process-global: first load wins. Safe to call defensively.
    tars.init(home=str(home))
    return home


@pytest.fixture(scope="module")
def workspace(tmp_path_factory) -> Path:
    ws = tmp_path_factory.mktemp("workspace")
    marker = ws / ".arc"
    marker.mkdir()
    (marker / "config.toml").write_text(_WORKSPACE_CONFIG)
    return ws


def test_init_is_idempotent_and_reports_initialized(tars_home):
    assert tars.is_initialized() is True
    # Second call is a no-op (first load wins), not an error.
    tars.init(home=str(tars_home))
    assert tars.is_initialized() is True


def test_tars_home_resolves_the_flag(tars_home):
    # resolve_home returns the explicit flag verbatim.
    assert tars.tars_home(home=str(tars_home)) == str(tars_home)


def test_workspaces_open_returns_a_handle(tars_home, workspace):
    ws = tars.Workspaces("arc")
    assert ws.tool == "arc"
    handle = ws.open(str(workspace))
    assert isinstance(handle, tars.Tars)
    # open is cached: the same root returns a handle for the same workspace.
    again = ws.open(str(workspace))
    assert os.path.realpath(handle.root) == os.path.realpath(str(workspace))
    assert again.root == handle.root
    ws.close_all()


def test_handle_pipeline_and_provider_resolve_role(tars_home, workspace):
    ws = tars.Workspaces("arc")
    handle = ws.open(str(workspace))

    # layer 2 — middleware-wrapped, sink wired from the workspace store.
    pipe = handle.pipeline("critic")
    assert isinstance(pipe, tars.Pipeline)
    assert pipe.id == "echo"
    assert "telemetry" in pipe.layer_names
    assert "retry" in pipe.layer_names

    # layer 1 — raw provider.
    prov = handle.provider("critic")
    assert isinstance(prov, tars.Provider)
    assert prov.id == "echo"

    # inspect the [roles] mapping without building.
    assert handle.role_provider("critic") == "echo"
    ws.close_all()


def test_unknown_role_raises_typed_handle_error(tars_home, workspace):
    ws = tars.Workspaces("arc")
    handle = ws.open(str(workspace))
    with pytest.raises(tars.TarsHandleError) as ei:
        handle.pipeline("no_such_role")
    # TarsHandleError is rooted at TarsError and carries a typed `kind`.
    assert isinstance(ei.value, tars.TarsError)
    assert ei.value.kind == "unknown_role"
    assert ei.value.role == "no_such_role"
    ws.close_all()


def test_context_manager_enters_and_exits(tars_home, workspace):
    ws = tars.Workspaces("arc")
    handle = ws.open(str(workspace))
    with handle.context(session="sess-abc", tags=["dogfood"], tenant="acme") as guard:
        # Inside the block the pipeline builds/binds; no network call is made.
        assert handle.pipeline("critic").id == "echo"
        assert guard is not None
    ws.close_all()


def test_workspaces_close_evicts_handle(tars_home, workspace):
    ws = tars.Workspaces("arc")
    ws.open(str(workspace))
    assert any(os.path.realpath(r) == os.path.realpath(str(workspace)) for r in ws.roots())
    ws.close(str(workspace))
    assert ws.get(str(workspace)) is None


def test_workspaces_context_manager_closes_all(tars_home, workspace):
    with tars.Workspaces("arc") as ws:
        ws.open(str(workspace))
        assert len(ws.roots()) == 1
    # __exit__ ran close_all.
    assert ws.roots() == []


def test_standalone_handle_without_workspace(tars_home):
    handle = tars.Tars.standalone("arc", session="smoke-sess")
    assert isinstance(handle, tars.Tars)
    # standalone still resolves roles from the global [roles]/registry; there
    # is no workspace overlay, so the single configured provider answers.
    assert handle.role_provider("echo") == "echo"
    handle.close()
