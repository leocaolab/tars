//! The [`CliDialect`] trait (Doc 32 §5 C1): the per-CLI behavior seam.
//!
//! One `CliDialect` describes everything that varies between delegate
//! CLIs — the argv (per-CLI flags), where the prompt goes, how the CLI
//! emits its answer, and how to map that answer onto canonical
//! [`ChatEvent`]s. The shared [`super::AgentCliBackend`] owns the invariant
//! machinery (spawn + OS sandbox + stream drain) and delegates every
//! per-CLI decision to the dialect. Adding a CLI = one small impl.

use serde_json::Value;

use tars_types::{ChatEvent, ChatRequest, ProviderError, RequestContext, StopReason, Usage};

use super::argv::SubprocessInvocation;

/// The neutral, per-request invocation the backend hands to a dialect.
///
/// As-built (M0–M3) this is a type alias for the **still claude-shaped**
/// [`SubprocessInvocation`] that a [`SubprocessRunner`](super::SubprocessRunner)
/// consumes. Rather than generalizing the payload, M1 added
/// [`SubprocessInvocation::neutral`](super::SubprocessInvocation::neutral): the
/// non-claude dialects (gemini/codex/opencode/antigravity) fill only the neutral
/// fields (executable/model/prompt/timeout/env/cwd/sandbox) and leave the
/// claude-specific knobs at their defaults, each building its own argv in
/// [`CliDialect::argv`]. Generalizing the type so those inert claude fields go
/// away is still open (Doc 32 M4/G10).
pub type CliInvocation = SubprocessInvocation;

/// Where a dialect wants the prompt delivered to the child process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptChannel {
    /// Write the prompt to the child's stdin and close it (claude `-p -`).
    Stdin,
    /// Pass the prompt as a CLI argument (agy `-p "<prompt>"`).
    Arg,
    /// Write the prompt to a temp file and pass its path (`--prompt-file`).
    PromptFile,
}

/// How the CLI emits its answer — the axis that splits claude/opencode
/// (streamed JSON events) from `agy` (a single plain-text print).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputMode {
    /// The CLI emits JSON that the runner reconstructs into a value the
    /// dialect maps via [`CliDialect::parse_line`].
    JsonEvents,
    /// The CLI prints a plain-text answer the dialect maps via
    /// [`CliDialect::parse_text`].
    Text,
}

/// How the shared runner
/// ([`SharedCliRunner`](super::subprocess::SharedCliRunner)) reconstructs a
/// delegate's stdout into the single [`Value`] the dialect then maps. This is
/// the axis Doc 32 C2 factored out: the buffered CLI delegates differ only in
/// their OUTPUT FRAMING, so the dialect DECLARES its framing and ONE runner
/// serves every buffered CLI (FR-6 — add a CLI = a `CliDialect`, no bespoke
/// runner). The variants are the framings the delegates actually use:
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFraming {
    /// Parse the whole stdout as a single JSON object (`Value::Object`). When
    /// `strip_prefix` is set, drop any decorative bytes before the first `{`
    /// (gemini prints ripgrep/setup notices ahead of its JSON). claude also
    /// yields a single object, but its stream-json + child-reaper needs keep it
    /// on the dedicated [`RealSubprocessRunner`](super::subprocess::RealSubprocessRunner).
    SingleObject { strip_prefix: bool },
    /// Split stdout into NDJSON lines → `Value::Array` of raw line strings
    /// (codex, opencode); the dialect's [`CliDialect::parse_line`] maps each.
    JsonLinesArray,
    /// Return the raw stdout verbatim as a `Value::String` — an
    /// [`OutputMode::Text`] delegate (antigravity) prints a plain answer, no
    /// JSON; the backend hands the string to [`CliDialect::parse_text`].
    RawText,
}

/// Per-CLI behavior seam. `Send + Sync` so the backend can hold it behind
/// an `Arc` and share it across the stream future.
pub trait CliDialect: Send + Sync {
    /// Executable + flags for this CLI (the per-CLI argv, without the
    /// executable itself).
    fn argv(&self, inv: &CliInvocation) -> Vec<String>;

    /// Assemble the per-request invocation (executable, model, serialized
    /// prompt, env-strip table, cwd, and any per-CLI flags) from the
    /// request. Returns a typed error if the request can't be honored
    /// (e.g. a non-explicit model).
    ///
    /// M0 seam: the invocation is the claude-shaped [`CliInvocation`]; M1
    /// splits the neutral fields (built by the backend) from the per-CLI
    /// flags (contributed here).
    fn invocation(
        &self,
        req: &ChatRequest,
        model: &str,
        ctx: &RequestContext,
    ) -> Result<CliInvocation, ProviderError>;

    /// Where the prompt goes.
    fn prompt_channel(&self) -> PromptChannel;

    /// How the CLI emits its answer — the backend reads JSON + calls
    /// [`Self::parse_line`], or drains stdout + calls [`Self::parse_text`].
    fn output_mode(&self) -> OutputMode;

    /// How the shared [`SharedCliRunner`](super::subprocess::SharedCliRunner)
    /// frames this CLI's stdout into a [`Value`] (see [`OutputFraming`]).
    /// Default: a single JSON object with no prefix strip. Each buffered dialect
    /// declares its framing so ONE runner serves them all (FR-6). claude
    /// declares its framing too but is served by the streaming-capable
    /// [`RealSubprocessRunner`](super::subprocess::RealSubprocessRunner).
    fn output_framing(&self) -> OutputFraming {
        OutputFraming::SingleObject { strip_prefix: false }
    }

    /// `JsonEvents`: one reconstructed JSON value → the content
    /// [`ChatEvent`]s (0..N `Delta`/`ThinkingDelta` + the terminal
    /// `Finished`). The backend prepends the `Started` event and applies
    /// the caller's output-budget clamp, so this method owns only the
    /// parse. A shape it can't read → a typed error **carrying the raw
    /// line** (CLAUDE.md #1 / FR-2).
    fn parse_line(&self, _raw: &Value) -> Result<Vec<ChatEvent>, ProviderError> {
        unimplemented!("parse_line is only for OutputMode::JsonEvents dialects")
    }

    /// `Text`: the whole stdout → content [`ChatEvent`]s. Default: one
    /// `Delta` carrying the raw text + a natural `Finished`. `agy -p`
    /// (M3) uses this.
    fn parse_text(&self, stdout: &str) -> Result<Vec<ChatEvent>, ProviderError> {
        Ok(vec![
            ChatEvent::Delta {
                text: stdout.to_string(),
            },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ])
    }

    /// Optional additive env the CLI needs to pass through (e.g.
    /// `GEMINI_API_KEY` / `ANTIGRAVITY_API_KEY`). Default: none. Claude
    /// STRIPS auth env (via `SubprocessInvocation::stripped_env`) rather
    /// than adding any, so it returns `&[]`.
    fn env(&self) -> &[&str] {
        &[]
    }

    /// Absolute paths to the CLI's own persistent state/cache/log/socket
    /// directories that the workspace-write jail must ALSO make writable — the
    /// codex sandbox model: a `workspace-write` jail allows the workspace + real
    /// `$TMPDIR` + `/tmp` + **the CLI's own state dir**. `$TMPDIR`/`/tmp` are
    /// added centrally by the delegate spawn ([`default_tmp_writable_roots`]);
    /// this returns the per-CLI additions (e.g. opencode's
    /// `~/.local/share/opencode` log dir, codex's `~/.codex`), with `~` already
    /// resolved. The spawn skips any entry that does not exist, so returning a
    /// candidate that is absent is harmless.
    ///
    /// Default: none — claude / gemini / antigravity need only the worktree +
    /// `$TMPDIR`, which the spawn already grants.
    ///
    /// [`default_tmp_writable_roots`]: tars_sandbox::default_tmp_writable_roots
    fn state_dirs(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}
