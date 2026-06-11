"""tars wiring for the interview demo — the heart of the redesign.

This single file replaces the project's old `llm/` package (an `LLMClient`
ABC + a `factory` + hand-written `anthropic_client` / `openai_client` /
`gemini_client` / `mock_client`). All of that is now one tars `Pipeline`.

What tars gives the demo for free, that the bespoke layer hand-rolled:

  - **Provider-agnostic routing.** Swap Anthropic ↔ OpenAI ↔ Gemini ↔ a local
    llama.cpp/MLX model by changing `$TARS_PROVIDER` — no new client class.
  - **Native structured output.** `complete(response_schema=...)` enforces the
    Evaluator's JSON shape at the provider. The old evaluator stuffed the JSON
    schema into the prompt, then stripped ```json fences, then re-validated by
    hand. All three steps are gone — see `evaluator.py`.
  - **Typed errors.** `tars.TarsProviderError.kind` ("rate_limited" / "auth" /
    …) instead of `except Exception` + string matching.
  - **Observability built in.** Point a role at an event store and every call is
    queryable afterward with `tars events list` / `tars events show --with-bodies`.

The Blackboard itself (`models.py`) is the same single-source-of-truth pattern
tars documents as the Blackboard Pipeline (see docs/architecture/19).
"""

from __future__ import annotations

import os
from typing import Optional

import tars


class LlmRole:
    """A provider+model-bound tars `Pipeline`, shared by the demo's agents.

    Thin on purpose: it pins the model, picks loose structured-output mode
    (kinder to local GBNF servers — see docs/benchmarks/local-llm-bench.md),
    and maps tars' typed errors to a plain message the engine can log. The
    agents call `complete(...)` and stay readable.
    """

    def __init__(self, pipeline: "tars.Pipeline", model: str):
        self._pipeline = pipeline
        self._model = model

    def complete(
        self,
        *,
        system: Optional[str] = None,
        user: Optional[str] = None,
        messages: Optional[list[dict[str, str]]] = None,
        response_schema: Optional[dict] = None,
        max_output_tokens: int = 2048,
    ) -> str:
        """One completion. Pass `user=` (single-turn) OR `messages=` (multi-turn).

        With `response_schema` the provider is steered to emit exactly that JSON
        shape; the caller can `model_validate_json` the result without cleanup.
        """
        try:
            resp = self._pipeline.complete(
                model=self._model,
                system=system,
                user=user,
                messages=messages,
                response_schema=response_schema,
                # Loose mode: local llama.cpp/LM-Studio GBNF can lose recall under
                # strict grammar constraint; loose still steers shape. Harmless
                # (ignored) when response_schema is None.
                response_schema_strict=False,
                max_output_tokens=max_output_tokens,
            )
        except tars.TarsProviderError as e:
            raise RuntimeError(f"LLM call failed (kind={e.kind}): {e}") from e
        return resp.text


def build_role(
    provider: Optional[str] = None,
    model: Optional[str] = None,
    event_store_dir: Optional[str] = None,
) -> LlmRole:
    """Build the shared role from `$TARS_PROVIDER` / `$TARS_MODEL` (or args).

    Run `tars init` once to write `~/.tars/config.toml`, then point this at any
    provider in it. `event_store_dir` (or `$TARS_EVENT_STORE`) makes every call
    inspectable later via the `tars events` CLI.
    """
    provider = provider or os.environ.get("TARS_PROVIDER", "anthropic")
    model = model or os.environ.get("TARS_MODEL")
    if not model:
        raise SystemExit(
            "set $TARS_MODEL (e.g. `export TARS_MODEL=claude-sonnet-4-5`) "
            "or pass --model; tars needs an explicit model id."
        )
    pipeline = tars.Pipeline.from_default(
        provider,
        event_store_dir=event_store_dir or os.environ.get("TARS_EVENT_STORE"),
    )
    return LlmRole(pipeline, model)
