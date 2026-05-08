"""TARS — Rust-backed multi-provider LLM runtime exposed as a Python package.

Three layered surfaces:

- ``Provider`` — raw backend, no middleware. Use when you want to manage
  cache / retry / circuit breaker yourself in Python.
- ``Pipeline`` — middleware-wrapped backend (cache + retry + telemetry
  engaged automatically). The common case for production use.
- ``run_task`` (deferred to a future commit) — full agent runtime
  (Orchestrator → Worker → Critic) exposed as a single function call.

The ``tars._tars_py`` submodule is the compiled extension and is
considered private.
"""

from tars._tars_py import (
    Annotate,
    CapabilityRequirements,
    CompatibilityReason,
    CompatibilityResult,
    FilterText,
    Pass,
    Pipeline,
    Provider,
    Reject,
    Response,
    RetryAttempt,
    Session,
    TarsConfigError,
    TarsError,
    TarsProviderError,
    TarsRoutingExhaustedError,
    TarsRuntimeError,
    Telemetry,
    Usage,
    default_config_path,
    version,
)

__all__ = [
    "Annotate",
    "CapabilityRequirements",
    "CompatibilityReason",
    "CompatibilityResult",
    "FilterText",
    "Pass",
    "Pipeline",
    "Provider",
    "Reject",
    "Response",
    "RetryAttempt",
    "Session",
    "TarsConfigError",
    "TarsError",
    "TarsProviderError",
    "TarsRoutingExhaustedError",
    "TarsRuntimeError",
    "Telemetry",
    "Usage",
    "default_config_path",
    "version",
]
