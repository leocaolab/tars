//! Invocation-shape types and argv construction for the Claude CLI
//! backend. Holds the "what flags go where" knowledge: the
//! [`SubprocessRunner`] trait every runner implements, the
//! [`SubprocessInvocation`] payload, the env-strip table, and the
//! pure functions ([`build_argv`], [`build_argv_with`],
//! [`streaming_enabled`]) that translate a builder configuration into
//! the exact tokens that follow `claude`.
//!
//! Pulled out of the original 1328-line `claude_cli.rs` so that the
//! argv-shape unit tests can exercise this layer without dragging in
//! `tokio::process::Command` or a real `claude` binary.

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use tars_types::{ChatRequest, ContentBlock, Message, ProviderError};

/// Env vars that must NEVER leak into the child `claude` process.
/// Case-insensitive — Windows preserves env var case, so `Anthropic_Api_Key`
/// would slip past a literal-equality check (the Python comment is exactly
/// about this hazard).
pub(super) const STRIPPED_ENV_KEYS_UPPER: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// What to pass via `--tools` on the CLI argv.
///
/// `Disabled` is the safe default — without any tools available the
/// CLI cannot start its internal agent loop, so a `--tools ""` call
/// is a pure inference channel and is **auth-neutral**. See
/// [docs/architecture/01-llm-provider.md §17](../../../../docs/architecture/01-llm-provider.md)
/// for the design rationale and the token-inflation data that motivated it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ClaudeCliTools {
    /// `--tools ""` — disable every tool. No agent loop possible.
    #[default]
    Disabled,
    /// Omit `--tools` entirely — inherit the CLI's default (full tool access).
    Default,
    /// `--tools "<csv>"` — allow only the named tools (e.g. `["Read","Bash"]`).
    Allow(Vec<String>),
}

/// What to pass via `--effort` on the CLI argv. `None` means omit the flag
/// and let the CLI use its own default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeCliEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ClaudeCliEffort {
    pub(super) fn as_arg(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Single CLI invocation — what [`SubprocessRunner`] needs to know.
#[derive(Clone, Debug)]
pub struct SubprocessInvocation {
    pub executable: String,
    pub model: String,
    pub system: Option<String>,
    pub prompt: String,
    pub timeout: Duration,
    /// Env vars to strip from the child (UPPER-CASE for case-insensitive match).
    pub stripped_env: HashSet<String>,
    /// `--tools` knob — see [`ClaudeCliTools`].
    pub tools: ClaudeCliTools,
    /// `--bare` — see the builder doc for the auth caveat.
    pub bare: bool,
    /// `--effort <level>` — `None` omits the flag.
    pub effort: Option<ClaudeCliEffort>,
    /// `--exclude-dynamic-system-prompt-sections`.
    pub exclude_dynamic_sections: bool,
    /// Raw argv tokens appended at the very end. Escape hatch.
    pub extra_args: Vec<String>,
}

/// Abstraction for "run `claude` and get back its JSON payload".
/// Production impl spawns a real subprocess; tests substitute a fake.
#[async_trait]
pub trait SubprocessRunner: Send + Sync {
    async fn run(&self, invocation: SubprocessInvocation) -> Result<Value, ProviderError>;
}

/// True iff the env var `TARS_CLAUDE_CLI_STREAM` is set to a non-empty,
/// non-zero, non-"false" value. Triggers stream-json mode in [`build_argv`]
/// and live-event mirroring in the production runner.
///
/// Stream mode is opt-in (off by default) so existing callers that depend
/// on the buffered `--output-format json` shape are unaffected.
pub(crate) fn streaming_enabled() -> bool {
    // The TARS_CLAUDE_CLI_STREAM read lives in tars-types::env so every
    // process-wide knob is greppable from a single import path
    // (ARC-L5-COH-18). Default-false semantics — operator must opt in.
    // A non-UTF-8 value (Err) is a misconfiguration: log it loudly
    // rather than silently treating it as unset.
    match tars_types::env::claude_cli_streaming() {
        Ok(opt) => opt.unwrap_or(false),
        Err(e) => {
            tracing::warn!(error = %e, "TARS_CLAUDE_CLI_STREAM set but unreadable (non-UTF-8); ignoring");
            false
        }
    }
}

/// Construct the full `claude` argv (without the executable itself) for
/// a given [`SubprocessInvocation`]. Shared between the production
/// runner and the argv-shape tests — that's the whole point of
/// factoring this out: when Anthropic renames a flag, exactly one
/// place changes and every test covering that flag fails immediately.
///
/// Output format is `json` by default. When `TARS_CLAUDE_CLI_STREAM` is
/// set, the CLI is invoked with `stream-json` + `--include-partial-messages`
/// + `--verbose`, which produces a real-time NDJSON event stream
///   (the runner tees each event to stderr for observability,
///   reconstructs the final `result` event as the return Value).
// Production now calls `build_argv_with` directly (reading
// `streaming_enabled()` exactly once — see `RealSubprocessRunner::run`).
// This convenience wrapper remains for the argv unit tests.
#[allow(dead_code)]
pub(super) fn build_argv(inv: &SubprocessInvocation) -> Vec<String> {
    build_argv_with(inv, streaming_enabled())
}

/// Inner constructor used by tests + by [`build_argv`] (which is the
/// production wrapper that reads `streaming_enabled()` from env). Pulled
/// out so tests can exercise both modes without process-global env
/// mutation (workspace forbids `unsafe`; Rust 2024 makes `env::set_var`
/// `unsafe`).
pub(super) fn build_argv_with(inv: &SubprocessInvocation, streaming: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "-p".into(),
        "-".into(),
        "--model".into(),
        inv.model.clone(),
        "--output-format".into(),
        if streaming {
            "stream-json".into()
        } else {
            "json".into()
        },
        "--disable-slash-commands".into(),
    ];
    if streaming {
        // --verbose is REQUIRED for the CLI to emit the per-event stream
        // alongside stream-json; without it, the result is the same single
        // payload as plain json. --include-partial-messages adds the
        // text_delta / thinking_delta chunks (the "live console" effect).
        argv.push("--include-partial-messages".into());
        argv.push("--verbose".into());
    }

    match &inv.tools {
        ClaudeCliTools::Disabled => {
            argv.push("--tools".into());
            argv.push(String::new());
        }
        ClaudeCliTools::Default => { /* omit --tools entirely */ }
        ClaudeCliTools::Allow(list) => {
            argv.push("--tools".into());
            // `--tools` is a single comma-joined value, so a comma inside
            // a tool name would silently split it into two bogus tools.
            // Drop any such name (with a warning) rather than corrupt the
            // whitelist — a name containing a comma can never be a valid
            // CLI tool identifier anyway.
            let cleaned: Vec<&String> = list
                .iter()
                .filter(|name| {
                    if name.contains(',') {
                        tracing::warn!(
                            tool = %name,
                            "claude_cli: dropping tool name containing ',' — it would corrupt the --tools CSV",
                        );
                        false
                    } else {
                        true
                    }
                })
                .collect();
            argv.push(
                cleaned
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
    }

    if inv.bare {
        argv.push("--bare".into());
    }

    if let Some(e) = inv.effort {
        argv.push("--effort".into());
        argv.push(e.as_arg().into());
    }

    if inv.exclude_dynamic_sections {
        argv.push("--exclude-dynamic-system-prompt-sections".into());
    }

    if let Some(sys) = &inv.system {
        argv.push("--system-prompt".into());
        argv.push(sys.clone());
    }

    argv.extend(inv.extra_args.iter().cloned());

    argv
}

/// Flatten our message history into the single text blob the CLI expects.
/// Mirrors the Python `chat_multi` serializer ([role]\n content per turn).
///
/// **Known limitation — role-marker ambiguity.** The CLI's `-p` mode
/// accepts only a single flat prompt string, so multi-turn history is
/// encoded with `[role]` line markers. This transport is inherently
/// lossy: message content that itself contains a line like `[user]` is
/// indistinguishable from a real turn boundary once flattened. There is
/// no out-of-band channel to disambiguate (the CLI has no structured
/// multi-message stdin), so callers that need a faithful multi-turn
/// transcript should prefer a structured backend (HTTP / claude_sdk).
/// For the single-turn case this backend is mainly used for, the risk
/// is moot — there are no intermediate boundaries to spoof.
pub(super) fn serialize_messages_for_cli(req: &ChatRequest) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(req.messages.len());
    for m in &req.messages {
        let (role, content) = match m {
            Message::User { content } => ("user", content),
            Message::Assistant { content, .. } => ("assistant", content),
            Message::Tool { content, .. } => ("tool", content),
            Message::System { content } => ("system", content),
        };
        let flat = flatten_blocks(content);
        parts.push(format!("[{role}]\n{flat}"));
    }
    parts.join("\n\n")
}

fn flatten_blocks(blocks: &[ContentBlock]) -> String {
    let mut out: Vec<String> = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text { text } => out.push(text.clone()),
            ContentBlock::Image { mime, .. } => {
                // Strip `]` (and stray brackets) from the mime so it can't
                // prematurely close the `[image:...]` marker — the CLI is
                // text-only here, so this is purely a readable placeholder.
                let safe: String = mime.chars().filter(|c| *c != ']' && *c != '[').collect();
                out.push(format!("[image:{safe}]"));
            }
        }
    }
    out.join("\n")
}
