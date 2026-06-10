//! View layer — **vendored from `openai/codex` `codex-rs/tui`** (Apache-2.0),
//! adapted to upstream ratatui 0.29 (Codex pins nornagon forks; we de-forked).
//! See `NOTICE`. This is the chat transcript's *look* — the markdown render
//! pipeline, text wrapping, ansi handling. The controller that drives it is
//! TARS-native and lives outside this module.
//!
//! Lints are relaxed here: this is third-party code kept close to upstream so
//! it stays easy to re-sync; our own code (the controller) is held to the
//! normal bar.

pub mod ansi_escape;
pub mod line_utils;
pub mod markdown;
pub mod markdown_render;
pub mod markdown_stream;
pub mod text_formatting;
pub mod wrapping;
