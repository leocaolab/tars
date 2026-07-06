//! Production [`SubprocessRunner`]: spawns the delegate CLI for real,
//! wraps it in `tars-sandbox` (Doc 29), and parses its
//! `--output-format json` payload. The stream-json path lives in
//! [`super::streaming`]; the buffered path is right here. Holds the
//! JSON-shape helpers (`extract_result_text`, `extract_usage`,
//! `truncate`) shared with the streaming code.
//!
//! Lifted verbatim from `claude_cli/subprocess.rs` (Doc 32 §7) so the
//! spawn + OS-sandbox wrap + line-drain are the ONE shared machinery for
//! every `CliDialect`. The `security_delegate_cli` integration test drives
//! this `run` directly. Since the default-confine change (Doc 32 FR-3) a
//! delegate is OS-sandboxed **unconditionally** — the jail root is the
//! worktree cwd (else the process cwd) — so confinement no longer depends on
//! the legacy `TARS_CLAUDE_SANDBOX` env gate. The workspace-write jail follows
//! the codex model: writable = worktree + real `$TMPDIR` + `/tmp` + the CLI's
//! own state dir, with `<worktree>/.git` write-protected.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use tars_types::{ProviderError, Usage};

use crate::child_reaper::ReaperGuard;

use super::argv::{SubprocessInvocation, SubprocessRunner, build_argv_with, streaming_enabled};
use super::streaming::run_streaming;

/// Resolve the effective OS-confinement + jail root for a delegate spawn
/// (Doc 32 FR-3 / NFR-2). A CLI delegate is a **black-box coding agent** (it
/// reads/writes files and runs bash), so it is OS-sandboxed **by default** — it
/// is never run unconfined. Returns the `(effective_policy, jail_root)` to hand
/// to [`tars_sandbox::SandboxPolicy::wrap`].
///
/// Precedence:
///
/// 1. **Jail root ("effective cwd").** The invocation's worktree `cwd` if set,
///    else the process cwd (the SAFE fallback for a delegate spawned without a
///    per-step worktree — e.g. bare `run -P claude_cli`). If neither resolves we
///    **fail closed** (refuse the spawn) rather than run a black box unconfined
///    (CLAUDE.md #1: the failure is truth, don't cover it by widening).
/// 2. **An explicit confining policy** (`ReadOnly` / `WorkspaceWrite` from
///    `[sandbox]`/`--sandbox`) is honored as-is; a `WorkspaceWrite` with no
///    configured roots is rooted at the jail root (matching `resolve_step_sandbox`
///    for tools).
/// 3. **`DangerFullAccess`** — the [`SandboxPolicy`](tars_sandbox::SandboxPolicy)
///    default AND the explicit `--sandbox danger-full-access` — is **downgraded**
///    to a `workspace-write` jail rooted at the jail root. This is the default-
///    confine flip: a delegate must never run unconfined, so an unset policy
///    (the common case) and an explicit "full access" alike become a worktree
///    write-jail. The downgrade is **logged** (truth, not silence). The caller's
///    network toggle is carried through.
///
/// The legacy `TARS_CLAUDE_SANDBOX=1` env gate is no longer read: confinement is
/// now unconditional, so setting it is a redundant no-op (it "still works" in
/// that the delegate is confined either way).
fn resolve_effective_policy(
    policy: &tars_sandbox::SandboxPolicy,
    cwd: Option<&Path>,
) -> Result<(tars_sandbox::SandboxPolicy, PathBuf), ProviderError> {
    use tars_sandbox::{SandboxMode, SandboxPolicy};

    // 1. Jail root: worktree cwd, else process cwd (SAFE fallback), else fail closed.
    let jail_root: PathBuf = match cwd {
        Some(c) => c.to_path_buf(),
        None => std::env::current_dir().map_err(|e| {
            ProviderError::Internal(format!(
                "CLI delegate sandbox: no worktree cwd supplied and the process cwd is \
                 unresolvable ({e}) — refusing to run a black-box delegate unconfined \
                 (fail-closed, Doc 32 FR-3)"
            ))
        })?,
    };

    let effective = match policy.mode {
        // 2. Explicit confining policy — honored as-is; fill an empty
        //    workspace-write root set with the jail root.
        SandboxMode::WorkspaceWrite => {
            let mut p = policy.clone();
            if p.writable_roots.is_empty() {
                p.writable_roots = vec![jail_root.clone()];
            }
            p
        }
        SandboxMode::ReadOnly => policy.clone(),
        // 3. Default / explicit full-access → downgraded to a worktree write-jail.
        SandboxMode::DangerFullAccess => {
            if cwd.is_some() {
                tracing::debug!(
                    jail_root = %jail_root.display(),
                    "CLI delegate: DangerFullAccess downgraded to a workspace-write jail on \
                     the worktree — a black-box delegate is always OS-sandboxed (Doc 32 FR-3)"
                );
            } else {
                tracing::info!(
                    jail_root = %jail_root.display(),
                    "CLI delegate: no confining [sandbox] policy and no worktree cwd — \
                     confining the black-box delegate to a workspace-write jail rooted at \
                     the process cwd (default-confine, Doc 32 FR-3)"
                );
            }
            let mut p = SandboxPolicy::workspace_write(&jail_root);
            p.network = policy.network; // carry the caller's network toggle
            p
        }
    };
    Ok((effective, jail_root))
}

/// Build the OS-sandboxed [`Command`] for a delegate CLI — the ONE shared spawn
/// primitive every `CliDialect`'s runner reuses (Doc 32 §5 C2 / FR-3). Given the
/// already-built argv, it:
///
/// - Wraps the exec in `tars-sandbox`'s OS jail per the effective policy +
///   jail root ([`resolve_effective_policy`]). A delegate is confined **by
///   default** (an unset/`DangerFullAccess` policy is downgraded to a
///   workspace-write jail on the worktree); an explicit `ReadOnly`/
///   `WorkspaceWrite` policy is honored. Fail-closed: a wrap error (or a
///   platform with no sandbox impl) aborts the spawn rather than running
///   unconfined. This is how **gemini** (previously unsandboxed), **codex**
///   (defense-in-depth on top of its own `--sandbox`), and every other delegate
///   get the uniform OS jail.
/// - **Widens a workspace-write jail to the codex writable set**: the worktree
///   PLUS the real `$TMPDIR`, `/tmp` ([`tars_sandbox::default_tmp_writable_roots`]),
///   and the CLI's own `state_dirs` (codex's `~/.codex`, opencode's
///   `~/.local/share/opencode`, …). A black-box coding agent needs its scratch +
///   state dir; the old jail denied them (redirecting `TMPDIR` into the worktree),
///   which broke codex's app-server socket and opencode's log. Non-existent
///   entries are skipped so the wrap's canonicalize can't fail on them. `.git`
///   under the worktree stays write-protected — that deny lives in
///   [`tars_sandbox::SandboxPolicy::wrap`].
/// - Strips the dangerous env vars CASE-INSENSITIVELY; passes everything else,
///   INCLUDING the real `$TMPDIR` (no longer redirected into the worktree — the
///   jail now allows the real one, so the child and the jail agree).
///
/// The caller sets stdio + the prompt channel (stdin vs an argv token) and
/// spawns — that part varies per CLI, the jail does not. `state_dirs` come from
/// the [`CliDialect`](super::dialect::CliDialect) (empty for claude/gemini).
pub(crate) fn build_sandboxed_command(
    executable: &str,
    argv: &[String],
    stripped_env: &HashSet<String>,
    cwd: Option<&Path>,
    policy: &tars_sandbox::SandboxPolicy,
    state_dirs: &[PathBuf],
) -> Result<Command, ProviderError> {
    let (mut effective, jail_root) = resolve_effective_policy(policy, cwd)?;

    // codex model: a workspace-write delegate jail also allows the real
    // `$TMPDIR` + `/tmp` + the CLI's own state dir. Append them (deduped,
    // existing-only) so `wrap` re-allows each writable root. ReadOnly stays
    // empty — a reviewer gets no scratch.
    if effective.mode == tars_sandbox::SandboxMode::WorkspaceWrite {
        let extras = tars_sandbox::default_tmp_writable_roots()
            .into_iter()
            .chain(state_dirs.iter().filter(|p| p.exists()).cloned());
        for extra in extras {
            if !effective.writable_roots.contains(&extra) {
                effective.writable_roots.push(extra);
            }
        }
    }

    // Write-jail the delegate's process tree to the jail root. `wrap` is
    // fail-closed: an error here (bad path, unsupported platform) aborts the
    // spawn rather than running a black-box delegate unconfined.
    let (wrapper, wrapped_argv) = effective
        .wrap(executable, argv, &jail_root)
        .map_err(|e| ProviderError::Internal(format!("exec sandbox: {e}")))?;
    let mut cmd = Command::new(wrapper);
    for tok in wrapped_argv {
        cmd.arg(tok);
    }
    cmd.current_dir(&jail_root); // the delegate's Read/Edit/Bash default to the jail root

    // Strip the dangerous env vars CASE-INSENSITIVELY. Pass through everything
    // else — INCLUDING the real `$TMPDIR`/`TMP`/`TEMP`. The jail now grants the
    // real `$TMPDIR` (codex model), so the delegate's scratch and the jail's
    // writable set agree; we no longer redirect the child's tmp into the
    // worktree (that redirect was the workaround for the old tmp-denying jail).
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if !stripped_env.contains(&k.to_uppercase()) {
            cmd.env(k, v);
        }
    }

    Ok(cmd)
}

pub struct RealSubprocessRunner;

#[async_trait]
impl SubprocessRunner for RealSubprocessRunner {
    async fn run(&self, inv: SubprocessInvocation) -> Result<Value, ProviderError> {
        // Read the streaming flag ONCE and thread it consistently into
        // both argv construction and the execution-path branch below.
        // Reading `streaming_enabled()` twice is a TOCTOU race: if
        // `TARS_CLAUDE_CLI_STREAM` flips between the two reads the child
        // is spawned with `--output-format json` but parsed as
        // `stream-json` (or vice-versa), corrupting the result.
        let streaming = streaming_enabled();

        let argv = build_argv_with(&inv, streaming);

        // Exec sandbox (Doc 29) + env-strip + spawn: the shared primitive.
        // The delegate is jailed by DEFAULT (Doc 32 FR-3) — its process tree is
        // write-confined to the worktree cwd (else the process cwd) so it cannot
        // write beyond it (claude runs with `bypassPermissions`, so its own
        // confinement is off — the OS jail is the only structural boundary).
        // Fail-closed: a sandbox that can't be built aborts the spawn.
        let mut cmd = build_sandboxed_command(
            &inv.executable,
            &argv,
            &inv.stripped_env,
            inv.cwd.as_deref(),
            &inv.sandbox,
            // claude keeps its state under the worktree / `$TMPDIR`; no extra
            // per-CLI state dir (its dialect's `state_dirs` is empty).
            &[],
        )?;

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Put the child in its OWN process group (becomes group leader) so
        // the signal-time reaper can SIGKILL the whole subtree as a unit
        // (claude may itself fork helpers). kill_on_drop still covers the
        // graceful path; the process group covers the signal path.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn().map_err(|e| spawn_error(&inv.executable, e))?;

        // Register the PID so a SIGINT/SIGTERM reaper in the host can
        // SIGKILL this child's process group. The guard deregisters on
        // EVERY exit path of this function (early `?`, timeout, success),
        // mirroring kill_on_drop's graceful coverage.
        let _reaper_guard = child.id().map(ReaperGuard::new);

        // Write the prompt on stdin and close it. stdin must be present
        // (Stdio::piped above); if it isn't, fail loudly rather than
        // silently skip the write and let the child block on an EOF that
        // never comes until the timeout fires.
        let mut stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Internal("claude child has no stdin pipe (Stdio::piped above)".into())
        })?;
        {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(inv.prompt.as_bytes()).await.map_err(|e| {
                ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!(
                        "stdin write failed after {} prompt bytes: {e}",
                        inv.prompt.len()
                    ),
                }
            })?;
        }
        // dropping `stdin` here closes the pipe so the child sees EOF
        drop(stdin);

        // Streaming branch — `TARS_CLAUDE_CLI_STREAM=1`: read stdout line
        // by line as NDJSON events, tee a pretty per-event summary to
        // stderr, return the reconstructed `result` event so callers see
        // the same shape as buffered mode.
        if streaming {
            return run_streaming(&mut child, &inv).await;
        }

        // Wait with timeout.
        let output = match tokio::time::timeout(inv.timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!("wait failed: {e}"),
                });
            }
            Err(_) => {
                // Timed out. `wait_with_output()` owns `child`, so the
                // child is killed deterministically the moment the timed-
                // out future is dropped (we set `kill_on_drop(true)` at
                // spawn) — i.e. as this match arm returns. No leaked
                // process survives the timeout.
                return Err(ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!(
                        "timed out after {}s (model={}, prompt_chars={})",
                        inv.timeout.as_secs(),
                        inv.model,
                        inv.prompt.len()
                    ),
                });
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // UTF-8-safe truncation — byte-indexing `[..500]` panics if
            // byte 500 lands mid-codepoint (stderr can carry arbitrary
            // Unicode: paths, user messages).
            let truncated = truncate(&stderr, 500);
            return Err(ProviderError::CliSubprocessDied {
                exit_code: output.status.code(),
                stderr: truncated,
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let payload: Value = serde_json::from_str(&stdout).map_err(|e| {
            ProviderError::Parse(format!(
                "claude CLI non-JSON stdout: {e} (first 300: {})",
                truncate(&stdout, 300)
            ))
        })?;

        if !payload.is_object() {
            return Err(ProviderError::Parse(format!(
                "claude CLI returned non-object JSON ({:?})",
                payload
            )));
        }

        if payload
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let detail = payload
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("<no detail>")
                .to_string();
            return Err(ProviderError::CliSubprocessDied {
                exit_code: Some(0), // CLI signaled error in payload, not via exit code
                stderr: format!("claude CLI returned error: {}", truncate(&detail, 300)),
            });
        }

        Ok(payload)
    }
}

/// The ONE shared buffered runner for every non-streaming CLI delegate (Doc 32
/// C2 / FR-6). It owns the invariant machinery — build the argv (from the
/// dialect), OS-sandbox the spawn ([`build_sandboxed_command`]), feed the prompt
/// per [`PromptChannel`], drain stdout/stderr concurrently under the timeout,
/// then reconstruct stdout into a [`Value`] per the dialect's declared
/// [`OutputFraming`]. The FOUR framings the 5 legacy runners hand-rolled
/// (single object / prefix-stripped object / JSONL→array / raw text) are now a
/// single `match`, so a new buffered CLI needs only a [`CliDialect`] (argv +
/// parse + declared framing) — **no bespoke runner**.
///
/// claude is NOT served here: its `stream-json` NDJSON path ([`super::streaming`])
/// plus the child-reaper / process-group teardown it needs (claude forks
/// helpers) keep it on the dedicated [`RealSubprocessRunner`]. This runner
/// covers gemini / codex / opencode / antigravity.
pub struct SharedCliRunner {
    dialect: std::sync::Arc<dyn super::dialect::CliDialect>,
}

impl SharedCliRunner {
    pub fn new(dialect: std::sync::Arc<dyn super::dialect::CliDialect>) -> Self {
        Self { dialect }
    }
}

/// Human label for a delegate's diagnostics — the executable's basename
/// (`/opt/homebrew/bin/gemini` → `gemini`), falling back to the full string.
fn cli_label(executable: &str) -> String {
    Path::new(executable)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| executable.to_string())
}

/// Map a spawn failure to a typed [`ProviderError`] carrying the executable +
/// cause (CLAUDE.md #1). Shared by both runners.
fn spawn_error(executable: &str, e: std::io::Error) -> ProviderError {
    match e.kind() {
        std::io::ErrorKind::NotFound => ProviderError::CliSubprocessDied {
            exit_code: None,
            stderr: format!("`{executable}` not found in PATH"),
        },
        std::io::ErrorKind::PermissionDenied => ProviderError::CliSubprocessDied {
            exit_code: None,
            stderr: format!("`{executable}` not executable: {e}"),
        },
        _ => ProviderError::CliSubprocessDied {
            exit_code: None,
            stderr: format!("spawn failed: {e}"),
        },
    }
}

#[async_trait]
impl SubprocessRunner for SharedCliRunner {
    async fn run(&self, inv: SubprocessInvocation) -> Result<Value, ProviderError> {
        use super::dialect::{OutputFraming, PromptChannel};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let label = cli_label(&inv.executable);

        // Argv comes from the dialect (no per-CLI argv duplication in the runner).
        let argv = self.dialect.argv(&inv);
        // The dialect declares its own state/cache/log/socket dirs (codex's
        // `~/.codex`, opencode's `~/.local/share/opencode`); the jail makes them
        // writable so the black-box agent can write its state.
        let state_dirs = self.dialect.state_dirs();
        let mut cmd = build_sandboxed_command(
            &inv.executable,
            &argv,
            &inv.stripped_env,
            inv.cwd.as_deref(),
            &inv.sandbox,
            &state_dirs,
        )?;

        // Prompt channel: Stdin ⇒ pipe + write; Arg/PromptFile ⇒ prompt is in
        // the argv, so close stdin.
        let feed_stdin = self.dialect.prompt_channel() == PromptChannel::Stdin;
        cmd.stdin(if feed_stdin {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| spawn_error(&inv.executable, e))?;

        if feed_stdin {
            let mut stdin = child.stdin.take().ok_or_else(|| {
                ProviderError::Internal(format!("{label} child has no stdin pipe (piped above)"))
            })?;
            stdin.write_all(inv.prompt.as_bytes()).await.map_err(|e| {
                ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!("stdin write failed: {e}"),
                }
            })?;
            drop(stdin); // EOF so the child stops reading
        }

        // Drain stdout + stderr concurrently with the wait so a full pipe can't
        // deadlock the child, while keeping `child` borrowed so we can kill it
        // on timeout.
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let collect = async {
            let read_out = async {
                if let Some(p) = stdout_pipe.as_mut() {
                    let _ = p.read_to_end(&mut stdout_buf).await;
                }
            };
            let read_err = async {
                if let Some(p) = stderr_pipe.as_mut() {
                    let _ = p.read_to_end(&mut stderr_buf).await;
                }
            };
            let (status, _, _) = tokio::join!(child.wait(), read_out, read_err);
            status
        };

        let status = match tokio::time::timeout(inv.timeout, collect).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                return Err(ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!("wait failed: {e}"),
                });
            }
            Err(_) => {
                // Explicit kill — start_kill signals immediately; reap so we
                // don't leave a zombie (kill_on_drop covers the deferred path).
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(ProviderError::CliSubprocessDied {
                    exit_code: None,
                    stderr: format!(
                        "timed out after {}s (model={})",
                        inv.timeout.as_secs(),
                        inv.model
                    ),
                });
            }
        };

        if !status.success() {
            let stderr_text = String::from_utf8_lossy(&stderr_buf);
            let truncated = truncate(&stderr_text, 500);
            return Err(ProviderError::CliSubprocessDied {
                exit_code: status.code(),
                stderr: format!("{label} exited non-zero: {truncated}"),
            });
        }

        // Reconstruct stdout per the dialect's declared framing.
        let stdout = String::from_utf8_lossy(&stdout_buf);
        match self.dialect.output_framing() {
            OutputFraming::SingleObject { strip_prefix } => {
                // Optionally drop decorative bytes before the first `{` (gemini).
                let json_text = if strip_prefix {
                    &stdout[stdout.find('{').unwrap_or(0)..]
                } else {
                    stdout.as_ref()
                };
                let payload: Value = serde_json::from_str(json_text).map_err(|e| {
                    ProviderError::Parse(format!(
                        "{label} CLI non-JSON stdout: {e} (first 300: {})",
                        truncate(&stdout, 300)
                    ))
                })?;
                if !payload.is_object() {
                    return Err(ProviderError::Parse(format!(
                        "{label} CLI returned non-object JSON ({payload:?})"
                    )));
                }
                Ok(payload)
            }
            OutputFraming::JsonLinesArray => {
                // Keep each line as a raw string; the dialect's `parse_line`
                // applies its lenient/critical per-line handling unchanged.
                let lines: Vec<Value> =
                    stdout.lines().map(|l| Value::String(l.to_string())).collect();
                Ok(Value::Array(lines))
            }
            OutputFraming::RawText => {
                // OutputMode::Text: hand the raw stdout to the backend as a JSON
                // string; the dialect's `parse_text` turns it into Delta + Finished.
                Ok(Value::String(stdout.into_owned()))
            }
        }
    }
}

/// CLI puts the response in `.result`. Python uses `payload.get("result") or ""`
/// to coerce JSON-null to empty string — same behavior here.
pub(crate) fn extract_result_text(payload: &Value) -> String {
    payload
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub(crate) fn extract_usage(payload: &Value) -> Usage {
    let usage = match payload.get("usage").and_then(|u| u.as_object()) {
        Some(u) => u,
        None => return Usage::default(),
    };
    let field = |name: &str| usage.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
    // The `claude -p --output-format json` result carries Anthropic-shape
    // usage where `input_tokens` counts ONLY the fresh (uncached) prompt
    // tokens, disjoint from cache read/creation. tars's `Usage` convention
    // (see tars-types `Usage` / `Pricing::cost_for`) is OpenAI-shape:
    // `input_tokens` is the TOTAL prompt, with `cached_input_tokens` and
    // `cache_creation_tokens` as subsets. Fold cache into the total so the
    // subset invariant holds and billing is correct — same normalization as
    // `claude_sdk::normalize_usage`.
    let fresh = field("input_tokens");
    let cached = field("cache_read_input_tokens");
    let created = field("cache_creation_input_tokens");
    Usage {
        input_tokens: fresh + cached + created,
        output_tokens: field("output_tokens"),
        cached_input_tokens: cached,
        cache_creation_tokens: created,
        thinking_tokens: 0,
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    let trimmed = crate::http_base::truncate_utf8(s, max);
    if trimmed.len() == s.len() {
        s.to_string()
    } else {
        format!("{trimmed}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tars_sandbox::{SandboxMode, SandboxPolicy};

    // Default-confine (Doc 32 FR-3): a delegate is OS-sandboxed unconditionally.
    // An explicit `ReadOnly`/`WorkspaceWrite` policy is honored; the default /
    // explicit `DangerFullAccess` is DOWNGRADED to a workspace-write jail. These
    // cases are deterministic regardless of `TARS_CLAUDE_SANDBOX` (no longer read).

    // Regression: `claude -p --output-format json` reports Anthropic-shape
    // usage where `input_tokens` is fresh-only and cache read/creation are
    // disjoint. `extract_usage` must fold cache into the TOTAL `input_tokens`
    // so tars's subset invariant (`cached + creation <= input`) holds and
    // `Pricing::cost_for` neither panics (debug) nor mis-bills (release).
    // Mirrors the live panic: fresh=10, cache_read=23808.
    #[test]
    fn extract_usage_folds_cache_into_total_input_no_invariant_violation() {
        use tars_types::Pricing;

        let payload = serde_json::json!({
            "usage": {
                "input_tokens": 10,
                "output_tokens": 42,
                "cache_read_input_tokens": 23808,
                "cache_creation_input_tokens": 100,
            }
        });
        let usage = extract_usage(&payload);

        // Total prompt = fresh + cache_read + cache_creation.
        assert_eq!(usage.input_tokens, 10 + 23808 + 100);
        assert_eq!(usage.cached_input_tokens, 23808);
        assert_eq!(usage.cache_creation_tokens, 100);
        assert_eq!(usage.output_tokens, 42);

        // The documented subset invariant now holds.
        assert!(usage.cached_input_tokens + usage.cache_creation_tokens <= usage.input_tokens);

        // `cost_for` must not panic (debug_assert) and must bill correctly:
        // billable fresh input = total - cache = 10 tokens.
        let pricing = Pricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: 0.3,
            cache_creation_per_million: 3.75,
            thinking_per_million: 0.0,
        };
        let cost = pricing.cost_for(&usage);
        let expected = 10.0 * 3.0 / 1e6
            + 42.0 * 15.0 / 1e6
            + 23808.0 * 0.3 / 1e6
            + 100.0 * 3.75 / 1e6;
        assert!((cost.0 - expected).abs() < 1e-12, "cost {} != {expected}", cost.0);
    }

    #[test]
    fn explicit_workspace_write_fills_cwd_when_roots_empty() {
        let cwd = PathBuf::from("/repo/wt");
        let policy = SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            writable_roots: Vec::new(), // empty ⇒ runtime fills [jail root]
            network: true,
        };
        let (eff, root) = resolve_effective_policy(&policy, Some(&cwd)).expect("confining ⇒ Ok");
        assert_eq!(eff.mode, SandboxMode::WorkspaceWrite);
        assert_eq!(eff.writable_roots, vec![cwd.clone()]);
        assert_eq!(root, cwd);
    }

    #[test]
    fn explicit_workspace_write_keeps_configured_roots() {
        let cwd = PathBuf::from("/repo/wt");
        let roots = vec![PathBuf::from("/repo/wt"), PathBuf::from("/scratch")];
        let policy = SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            writable_roots: roots.clone(),
            network: false,
        };
        let (eff, _root) = resolve_effective_policy(&policy, Some(&cwd)).unwrap();
        assert_eq!(eff.writable_roots, roots, "configured roots are not overwritten");
        assert!(!eff.network, "network toggle carried through");
    }

    #[test]
    fn explicit_read_only_is_honored() {
        let cwd = PathBuf::from("/repo/wt");
        let (eff, root) =
            resolve_effective_policy(&SandboxPolicy::read_only(true), Some(&cwd)).unwrap();
        assert_eq!(eff.mode, SandboxMode::ReadOnly);
        assert!(eff.writable_roots.is_empty());
        assert_eq!(root, cwd);
    }

    #[test]
    fn confining_policy_without_cwd_falls_back_to_process_cwd() {
        // Frozen-bug fix (Doc 32 FR-3): a confining policy with no worktree cwd
        // USED to run unconfined (return None). That was the hole. Now the jail
        // root falls back to the process cwd — the delegate is still confined,
        // never run unconfined. The policy's own roots are preserved.
        let policy = SandboxPolicy::workspace_write(&PathBuf::from("/x"));
        let (eff, root) = resolve_effective_policy(&policy, None).expect("SAFE fallback ⇒ Ok");
        assert_eq!(eff.mode, SandboxMode::WorkspaceWrite);
        assert_eq!(eff.writable_roots, vec![PathBuf::from("/x")]);
        assert_eq!(root, std::env::current_dir().unwrap());
    }

    #[test]
    fn danger_full_access_downgrades_to_workspace_write_on_worktree() {
        // The default (and explicit `--sandbox danger-full-access`) policy is
        // DOWNGRADED to a workspace-write jail rooted at the worktree — a
        // black-box delegate is never unconfined (Doc 32 FR-3). Deterministic
        // regardless of the legacy env gate (no longer read).
        let cwd = PathBuf::from("/repo/wt");
        let (eff, root) = resolve_effective_policy(&SandboxPolicy::default(), Some(&cwd)).unwrap();
        assert_eq!(eff.mode, SandboxMode::WorkspaceWrite);
        assert_eq!(eff.writable_roots, vec![cwd.clone()]);
        assert_eq!(root, cwd);
    }

    #[test]
    fn danger_full_access_without_cwd_confines_to_process_cwd() {
        // No worktree cwd + default policy → confine to the process cwd (the SAFE
        // fallback that keeps a bare `run -P claude_cli` working, now sandboxed).
        let (eff, root) = resolve_effective_policy(&SandboxPolicy::default(), None).unwrap();
        assert_eq!(eff.mode, SandboxMode::WorkspaceWrite);
        let proc_cwd = std::env::current_dir().unwrap();
        assert_eq!(eff.writable_roots, vec![proc_cwd.clone()]);
        assert_eq!(root, proc_cwd);
    }

    #[test]
    fn danger_full_access_carries_network_toggle() {
        // The caller's network toggle survives the downgrade.
        let cwd = PathBuf::from("/repo/wt");
        let no_net = SandboxPolicy { network: false, ..SandboxPolicy::default() };
        let (eff, _) = resolve_effective_policy(&no_net, Some(&cwd)).unwrap();
        assert!(!eff.network, "network=false carried through the downgrade");
    }
}
