"""TARS — Rust-backed multi-provider LLM runtime exposed as a Python package.

This is the public surface; the ``tars._tars_py`` submodule is the
compiled extension and is considered private.
"""

from tars._tars_py import version

__all__ = ["version"]
