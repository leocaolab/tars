//! macOS link fix for the PyO3 extension-module cdylib.
//!
//! With the `extension-module` feature the CPython symbols (`_Py*`) are left
//! **undefined** in the `.dylib` — they resolve at import time, not link time.
//! `maturin` passes `-undefined dynamic_lookup` for us, but a plain
//! `cargo build --all-targets` (CI / local full builds) does not, so the cdylib
//! link fails on macOS with unresolved `_Py*` symbols.
//!
//! Fix: emit the flag ourselves — **scoped to this crate's cdylib, on macOS
//! only** (`rustc-cdylib-link-arg` applies to no other crate and no other target
//! type), so nothing global masks a real undefined-symbol error elsewhere.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-undefined");
        println!("cargo:rustc-cdylib-link-arg=dynamic_lookup");
    }
}
