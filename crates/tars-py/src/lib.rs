//! Python bindings for `tars` — initial scope is the audit-tool path:
//! a `Client` class with `chat` / `chat_multi` matching ARC's
//! `LLMClient` ABC so existing Python code (ARC, future agouflow)
//! can drop-in replace its per-provider Python clients with one
//! Rust-backed handle wired through TARS's full pipeline (cache /
//! retry / circuit breaker / routing).
//!
//! ## This commit's scope
//!
//! Skeleton only — `tars.version()` returning the workspace version
//! string. Proves:
//! - the maturin build pipeline works (cargo → wheel → import in
//!   Python),
//! - the abi3-py310 wheel imports across 3.10+ without per-version
//!   rebuild,
//! - the workspace integration (no clippy regressions on the rest of
//!   the stack from adding a cdylib crate).
//!
//! Real `Client` API lands in the next commit; doing the skeleton
//! first keeps the build / packaging plumbing diffs separate from
//! the API design diffs.
//!
//! ## Build + smoke test
//!
//! ```bash
//! # one-time: install maturin
//! pip install maturin   # or: uv tool install maturin
//!
//! # build + install in current Python env
//! cd crates/tars-py
//! maturin develop --release
//!
//! # smoke
//! python -c "import tars; print(tars.version())"
//! ```

use pyo3::prelude::*;

/// `tars.version() -> str` — workspace package version.
#[pyfunction]
fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// PyO3 module entry point. The symbol `PyInit__tars_py` (note the
/// leading underscore — `__` because Python adds one) is what the
/// pyproject.toml's `module-name = "tars._tars_py"` resolves to
/// after maturin places the .so inside the `tars/` package dir.
/// End-user code never imports this directly; `python/tars/__init__.py`
/// re-exports the symbols under the `tars` namespace.
#[pymodule]
fn _tars_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    Ok(())
}
