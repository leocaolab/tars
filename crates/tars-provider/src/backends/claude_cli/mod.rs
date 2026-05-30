//! Claude Code CLI as an LLM Provider — subscription path.
//!
//! Mirrors the Python `ClaudeSubprocessClient` in
//! the equivalent Python subprocess client:
//!
//! - Shells out to `claude -p - --model X --output-format json
//!   --disable-slash-commands` and feeds the prompt on stdin.
//! - **Strips** `ANTHROPIC_API_KEY` (case-insensitive) and 3rd-party
//!   routing env vars before exec'ing the child. If any leak through,
//!   `claude` switches to API-billing mode and silently bills the
//!   wrong account.
//! - Parses the single JSON output from the CLI; surfaces typed errors
//!   on non-zero exit / malformed JSON / `is_error: true` payload.
//! - **Not yet streaming** — Doc 01 §6.2 calls for a long-lived process
//!   pool with `--output-format stream-json`; that's the next iteration
//!   (Doc 01 §6.2.1). This first cut spawns per call.
//!
//! Testability: the actual `Command::output()` call is behind a
//! [`SubprocessRunner`] trait so tests substitute a fake without
//! needing the real `claude` binary installed.
//!
//! ## Module layout (split per `arc scan --judge` finding `ARC-L5-M-13`)
//!
//! Originally a single 1328-line file that mixed config enums, the
//! provider lifecycle, argv construction, the real-subprocess runner,
//! response parsing, and the stream-json event reader. The L5
//! Tribunal flagged it as a god-module ROT with the suggested action
//! "split into focused sub-modules." Done as advised, mirroring the
//! Batch 6 `gemini.rs` split:
//!
//! - [`provider`] — `ClaudeCliProvider`, `ClaudeCliProviderBuilder`,
//!   the `LlmProvider` impl, default capabilities, and the
//!   `claude_cli()` convenience helper.
//! - [`argv`] — invocation-shape types (`ClaudeCliTools`,
//!   `ClaudeCliEffort`, `SubprocessInvocation`), the
//!   `SubprocessRunner` trait, env-strip table, and the argv
//!   constructors (`build_argv` / `build_argv_with` /
//!   `streaming_enabled`). The "what flag goes where" layer.
//! - [`subprocess`] — `RealSubprocessRunner` (the production
//!   implementation), buffered-mode JSON parsing helpers
//!   (`extract_result_text`, `extract_usage`, `truncate`).
//! - [`streaming`] — `run_streaming` + `emit_event_summary` for
//!   `--output-format stream-json` mode, gated by
//!   `TARS_CLAUDE_CLI_STREAM`.
//!
//! ## `arc scan --judge` finding `ARC-L5-COH-19` (env + subprocess)
//!
//! This backend owns three `std::env` reads and one `Command::new`
//! site that the scan flagged as scattered cohesion: the
//! `TARS_CLAUDE_CLI_STREAM` feature flag, the `std::env::vars()`
//! sweep used to **strip** untrusted keys (the security boundary
//! commented above), and the `Command::new(&inv.executable)` for the
//! `claude -p` subprocess. They are deliberately co-located with the
//! backend that interprets them:
//!
//! - `TARS_CLAUDE_CLI_STREAM` is a per-backend toggle whose semantics
//!   ("stream-json vs single-blob JSON") only make sense alongside the
//!   `build_argv` shape; moving it to a typed-env config crate would
//!   scatter the var name from the call site that knows the trade-off.
//! - The `env::vars()` sweep is a security boundary, not generic
//!   environment access — the strip table (`STRIPPED_ENV_KEYS_UPPER`)
//!   is specific to this backend's auth-routing concerns and would
//!   not generalize to a shared helper.
//! - `Command::new(&inv.executable)` is the spawn site for *this*
//!   backend's CLI; each provider backend (claude / codex / gemini)
//!   legitimately spawns its own provider-specific executable, which
//!   the scan's `[coh] subprocess` row classifies as **essential**
//!   (the `claude_cli.rs` count is one of those essential sites).

mod argv;
mod provider;
mod streaming;
mod subprocess;

pub use argv::{ClaudeCliEffort, ClaudeCliTools, SubprocessInvocation, SubprocessRunner};
pub use provider::{ClaudeCliProvider, ClaudeCliProviderBuilder, claude_cli};
pub use subprocess::RealSubprocessRunner;
