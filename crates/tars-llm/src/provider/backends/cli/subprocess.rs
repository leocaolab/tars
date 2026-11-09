//! Spawns the CLIs for real inside the `tars-sandbox` jail: [`RealSubprocessRunner`] for claude,
//! [`SharedCliRunner`] for the rest. A workspace-write jail can write the worktree, the real
//! `$TMPDIR`, `/tmp` and the CLI's own state directories, never the git directories of the
//! worktree's repository.
//!
//! It does not read the CLI's events; the dialect does.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::Value;
use tokio::process::Command;

use super::cli_subprocess_died;
use crate::ProviderError;
use crate::provider::error::{ParseProblem, SandboxSetupError};
use tars_types::{JsonKind, Usage};

use crate::provider::child_reaper::ReaperGuard;

use super::claude_argv::{build_argv_with, streaming_enabled};
use super::invocation::{ChildIo, RunFailure, RunOutput, SubprocessInvocation, SubprocessRunner};
use super::stream_json_log::run_streaming;

/// Returns the policy a CLI runs under and the jail root. The root is the worktree `cwd`, else the
/// process cwd. `ReadOnly` is kept; `WorkspaceWrite` with no roots gets the jail root; and
/// `DangerFullAccess`, the default included, becomes a workspace-write jail on the root that keeps
/// the caller's network setting: a coding agent that edits files and runs bash never runs
/// unconfined.
///
/// # Errors
///
/// Fails with `Sandbox` when there is no `cwd` and the process cwd cannot be read.
fn resolve_effective_policy(
    policy: &tars_sandbox::SandboxPolicy,
    cwd: Option<&Path>,
) -> Result<(tars_sandbox::SandboxPolicy, PathBuf), ProviderError> {
    use tars_sandbox::SandboxPolicy;

    let jail_root: PathBuf = match cwd {
        Some(c) => c.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| ProviderError::Sandbox(SandboxSetupError::NoWorkingDirectory(e)))?,
    };

    let effective = match policy {
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            network,
        } if writable_roots.is_empty() => SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![jail_root.clone()],
            network: *network,
        },
        SandboxPolicy::WorkspaceWrite { .. } | SandboxPolicy::ReadOnly { .. } => policy.clone(),
        SandboxPolicy::DangerFullAccess { network } => {
            if cwd.is_some() {
                tracing::debug!(
                    jail_root = %jail_root.display(),
                    "CLI delegate: DangerFullAccess downgraded to a workspace-write jail on \
                     the worktree — a black-box delegate is always OS-sandboxed"
                );
            } else {
                tracing::info!(
                    jail_root = %jail_root.display(),
                    "CLI delegate: no confining [sandbox] policy and no worktree cwd — \
                     confining the black-box delegate to a workspace-write jail rooted at \
                     the process cwd (default-confine, FR-3)"
                );
            }
            SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![jail_root.clone()],
                network: *network,
            }
        }
    };
    Ok((effective, jail_root))
}

/// Names the variables every CLI is given whatever its dialect, upper-cased: where programs are,
/// who and where the user is, the temp directories, the terminal, the time zone, proxies and CA
/// bundles. Locale variables (`LANG`, `LANGUAGE`, `LC_*`) pass too.
const SYSTEM_ENV_KEYS_UPPER: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TERM",
    "TZ",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "__CF_USER_TEXT_ENCODING",
];

/// Returns whether the tars process's variable `name` reaches a CLI that `needs` the listed
/// variables beyond the system ones and is never given `stripped` (upper-cased). Names are
/// compared whatever their case, since Windows keeps a variable's case.
fn passes_env(name: &str, needs: &[&str], stripped: &HashSet<String>) -> bool {
    let upper = name.to_uppercase();
    let system = SYSTEM_ENV_KEYS_UPPER.contains(&upper.as_str())
        || upper == "LANG"
        || upper == "LANGUAGE"
        || upper.starts_with("LC_");
    let needed = needs.iter().any(|n| n.eq_ignore_ascii_case(name));
    (system || needed) && !stripped.contains(&upper)
}

/// Returns the jailed [`Command`] for a CLI, whose environment holds only the system variables
/// and those the CLI `needs`, never `stripped_env` (matched whatever their case); a provider key
/// or other credential of the tars process is not passed on. A
/// workspace-write jail can also write `$TMPDIR`, `/tmp` and those `state_dirs` that exist (the
/// wrap canonicalizes each root); the git directories of the worktree's repository stay
/// read-only, denied by [`tars_sandbox::SandboxPolicy::wrap`]. The caller sets stdio and spawns.
///
/// # Errors
///
/// Fails with `Internal` when there is no jail root or the jail cannot be built (including on a
/// platform without one): the CLI is not spawned unconfined.
pub(crate) fn build_sandboxed_command(
    executable: &str,
    argv: &[String],
    needs_env: &[&str],
    stripped_env: &HashSet<String>,
    cwd: Option<&Path>,
    policy: &tars_sandbox::SandboxPolicy,
    state_dirs: &[PathBuf],
) -> Result<Command, ProviderError> {
    let (mut effective, jail_root) = resolve_effective_policy(policy, cwd)?;

    // A read-only jail gets no scratch space.
    if let tars_sandbox::SandboxPolicy::WorkspaceWrite { writable_roots, .. } = &mut effective {
        let extras = tars_sandbox::default_tmp_writable_roots()
            .into_iter()
            .chain(state_dirs.iter().filter(|p| p.exists()).cloned());
        for extra in extras {
            if !writable_roots.contains(&extra) {
                writable_roots.push(extra);
            }
        }
    }

    let wrapped = effective
        .wrap(executable, argv, &jail_root)
        .map_err(|e| ProviderError::Sandbox(SandboxSetupError::Policy(e)))?;
    for dropped in &wrapped.dropped {
        tracing::warn!(
            root = %dropped.root.display(),
            error = %dropped.source,
            "CLI delegate sandbox: writable root cannot be resolved and was left out of the jail"
        );
    }
    let mut cmd = Command::new(wrapped.program);
    cmd.args(wrapped.args);
    cmd.current_dir(&jail_root);

    cmd.env_clear();
    cmd.envs(std::env::vars().filter(|(name, _)| passes_env(name, needs_env, stripped_env)));

    Ok(cmd)
}

pub struct RealSubprocessRunner;

#[async_trait]
impl SubprocessRunner for RealSubprocessRunner {
    async fn run(&self, inv: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
        let mut exchanged = ChildIo::default();
        match self.run_claude(inv, &mut exchanged).await {
            Ok(payload) => Ok(RunOutput { payload, exchanged }),
            Err(error) => Err(RunFailure { error, exchanged }),
        }
    }
}

impl RealSubprocessRunner {
    /// Runs claude, noting into `ex` each byte exchanged with it as it goes.
    async fn run_claude(
        &self,
        inv: SubprocessInvocation,
        ex: &mut ChildIo,
    ) -> Result<Value, ProviderError> {
        // Read once: the argv and the parser below must agree on the output format even if
        // `TARS_CLAUDE_CLI_STREAM` changes meanwhile.
        let streaming = streaming_enabled();

        let argv = build_argv_with(&inv, streaming);

        // With tools, claude runs with `bypassPermissions`, so the jail is its only boundary.
        let mut cmd = build_sandboxed_command(
            &inv.executable,
            &argv,
            super::claude_argv::NEEDED_ENV_KEYS,
            &inv.stripped_env,
            inv.cwd.as_deref(),
            &inv.sandbox,
            // claude's Bash tool creates `~/.claude/session-env/<uuid>` on first use; without
            // write access there it cannot run bash at all.
            &super::dialects::claude::claude_state_dirs(),
        )?;
        let argv = ChildIo::argv_of(cmd.as_std());

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Its own process group, so the signal-time reaper also kills the helpers claude forks.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn().map_err(|e| {
            // Under a jail the program run is the wrapper, not the CLI.
            let program = cmd.as_std().get_program().to_string_lossy().into_owned();
            spawn_error(&program, &inv.executable, e)
        })?;
        ex.argv = Some(argv);

        let group = child.id();
        let _reaper_guard = group.map(ReaperGuard::new);

        let mut stdin = child.stdin.take().ok_or_else(|| {
            ProviderError::Internal("claude child has no stdin pipe (Stdio::piped above)".into())
        })?;
        write_stdin(&mut stdin, inv.prompt.as_bytes(), ex)
            .await
            .map_err(|e| {
                cli_subprocess_died(
                    None,
                    format!(
                        "stdin write failed after {} of {} prompt bytes: {e}",
                        ex.stdin.len(),
                        inv.prompt.len()
                    ),
                )
            })?;
        drop(stdin);

        if streaming {
            return run_streaming(&mut child, group, &inv, ex).await;
        }

        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let end = {
            let mut stdout_pipe = child.stdout.take();
            let mut stderr_pipe = child.stderr.take();
            let reads = async {
                tokio::join!(
                    read_pipe(stdout_pipe.as_mut(), &mut stdout_buf),
                    read_pipe(stderr_pipe.as_mut(), &mut stderr_buf)
                )
            };
            wait_with_reads(&mut child, reads, inv.timeout).await
        };
        ex.stdout = stdout_buf.into();
        ex.stderr = stderr_buf.into();
        let status = match end {
            ChildEnd::TimedOut => {
                end_child(&mut child, group, true).await;
                return Err(ProviderError::TimedOut {
                    budget: inv.timeout,
                    detail: format!(
                        "claude CLI killed after wall-clock timeout (model={}, prompt_chars={})",
                        inv.model,
                        inv.prompt.len()
                    ),
                });
            }
            ChildEnd::Exited { status, reads } => {
                end_child(&mut child, group, false).await;
                let status =
                    status.map_err(|e| cli_subprocess_died(None, format!("wait failed: {e}")))?;
                if let Some((out, err)) = reads {
                    out.and(err).map_err(|e| {
                        cli_subprocess_died(
                            status.code(),
                            format!("reading claude's output failed: {e}"),
                        )
                    })?;
                }
                status
            }
        };

        if !status.success() {
            let stderr = String::from_utf8_lossy(&ex.stderr).to_string();
            let truncated = truncate(&stderr, 500);
            return Err(cli_subprocess_died(status.code(), truncated));
        }

        let stdout = String::from_utf8_lossy(&ex.stdout);
        let payload = stdout_object("claude CLI stdout", &stdout, &stdout)?;

        if let Some(detail) = reported_error(&payload) {
            // The CLI exited 0 and reported the error in its JSON.
            return Err(cli_subprocess_died(
                Some(0),
                format!("claude CLI returned error: {}", truncate(&detail, 300)),
            ));
        }

        Ok(payload)
    }
}

/// Runs codex, opencode and antigravity: spawns the jailed CLI in its own process group, feeds the
/// prompt by the dialect's [`PromptChannel`](super::dialect::PromptChannel), reads stdout and
/// stderr until the CLI exits under the timeout, and frames stdout by its
/// [`OutputFraming`](super::dialect::OutputFraming).
pub struct SharedCliRunner {
    dialect: std::sync::Arc<dyn super::dialect::CliDialect>,
}

impl SharedCliRunner {
    pub fn new(dialect: std::sync::Arc<dyn super::dialect::CliDialect>) -> Self {
        Self { dialect }
    }
}

/// How long a child's pipes are still read after it exits. A process the CLI left behind (a
/// server its Bash tool started) can hold a pipe open for ever, while the answer is in what the
/// CLI wrote before it exited.
pub(super) const PIPE_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// How a child's run ended, as [`wait_with_reads`] saw it.
pub(super) enum ChildEnd<T> {
    /// The child exited. `reads` is what the pipe readers returned, or `None` when something
    /// still held a pipe open [`PIPE_GRACE`] after the exit; what they read so far is kept by
    /// them either way.
    Exited {
        status: std::io::Result<std::process::ExitStatus>,
        reads: Option<T>,
    },
    /// The timeout passed before the child exited.
    TimedOut,
}

/// Waits for `child` to exit within `timeout` while driving `reads`, the readers of its pipes,
/// so a full pipe never blocks it. The end of the run is the child's exit, not the pipes'
/// end-of-file: after the exit `reads` gets [`PIPE_GRACE`] more and is then dropped.
pub(super) async fn wait_with_reads<F: std::future::Future>(
    child: &mut tokio::process::Child,
    reads: F,
    timeout: std::time::Duration,
) -> ChildEnd<F::Output> {
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::pin!(reads);
    let mut done = None;
    let status = loop {
        tokio::select! {
            // `Child::wait` is cancel-safe, so a new one each turn loses nothing.
            status = child.wait() => break status,
            out = &mut reads, if done.is_none() => done = Some(out),
            () = tokio::time::sleep_until(deadline) => return ChildEnd::TimedOut,
        }
    };
    let reads = match done {
        Some(out) => Some(out),
        None => tokio::time::timeout(PIPE_GRACE, reads).await.ok(),
    };
    ChildEnd::Exited { status, reads }
}

/// Reads `pipe` to its end into `buf`; an absent pipe reads nothing.
pub(super) async fn read_pipe<R>(pipe: Option<&mut R>, buf: &mut Vec<u8>) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    match pipe {
        Some(p) => p.read_to_end(buf).await.map(drop),
        None => Ok(()),
    }
}

/// Kills the process group the child led, so nothing the CLI started outlives the run, then kills
/// the child itself when `timed_out` (it may still run, and leads no group off unix) and reaps it.
/// `group` is the child's PID from its spawn, since an exited child no longer reports one.
pub(super) async fn end_child(
    child: &mut tokio::process::Child,
    group: Option<u32>,
    timed_out: bool,
) {
    if let Some(pid) = group {
        crate::provider::child_reaper::kill_group(pid);
    }
    if timed_out && let Err(e) = child.start_kill() {
        tracing::debug!(error = %e, "CLI child: kill after the timeout failed");
    }
    if let Err(e) = child.wait().await {
        tracing::debug!(error = %e, "CLI child: reaping after the run failed");
    }
}

/// Writes `bytes` to the child's stdin, noting into `ex` exactly the bytes the pipe took: all of
/// them, or those before the write failed.
async fn write_stdin(
    stdin: &mut tokio::process::ChildStdin,
    bytes: &[u8],
    ex: &mut ChildIo,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut written = 0;
    let result = loop {
        if written == bytes.len() {
            break Ok(());
        }
        match stdin.write(&bytes[written..]).await {
            Ok(0) => break Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => written += n,
            Err(e) => break Err(e),
        }
    };
    ex.stdin = Bytes::copy_from_slice(&bytes[..written]);
    result
}

/// Returns the JSON object `json_text` holds, a part of `stdout`, which a failure carries cut to
/// 300 bytes.
fn stdout_object(at: &'static str, json_text: &str, stdout: &str) -> Result<Value, ProviderError> {
    let unparsed = |problem| ProviderError::Parse {
        at,
        line: None,
        problem: Box::new(problem),
        raw: Some(truncate(stdout, 300)),
    };
    let payload: Value =
        serde_json::from_str(json_text).map_err(|e| unparsed(ParseProblem::Json(e)))?;
    if !payload.is_object() {
        return Err(unparsed(ParseProblem::WrongType {
            expected: JsonKind::Object,
            got: JsonKind::of(&payload),
        }));
    }
    Ok(payload)
}

/// Returns the executable's file name for messages, or the whole string when it has none.
fn cli_label(executable: &str) -> String {
    Path::new(executable)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| executable.to_string())
}

/// Returns the error of a failed spawn. `program` is what was executed, which under a jail is the
/// wrapper (`bwrap`, `sandbox-exec`); when it is not the CLI both are named, so a missing
/// bubblewrap is not read as a missing `claude`.
fn spawn_error(program: &str, delegate: &str, e: std::io::Error) -> ProviderError {
    let what = if program == delegate {
        format!("`{program}`")
    } else {
        format!("`{program}` (the sandbox wrapper for `{delegate}`)")
    };
    match e.kind() {
        std::io::ErrorKind::NotFound => {
            cli_subprocess_died(None, format!("{what} not found in PATH"))
        }
        std::io::ErrorKind::PermissionDenied => {
            cli_subprocess_died(None, format!("{what} not executable: {e}"))
        }
        _ => cli_subprocess_died(None, format!("spawn failed for {what}: {e}")),
    }
}

#[async_trait]
impl SubprocessRunner for SharedCliRunner {
    async fn run(&self, inv: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
        let mut exchanged = ChildIo::default();
        match self.run_delegate(inv, &mut exchanged).await {
            Ok(payload) => Ok(RunOutput { payload, exchanged }),
            Err(error) => Err(RunFailure { error, exchanged }),
        }
    }
}

impl SharedCliRunner {
    /// Runs the CLI, noting into `ex` each byte exchanged with it as it goes.
    async fn run_delegate(
        &self,
        inv: SubprocessInvocation,
        ex: &mut ChildIo,
    ) -> Result<Value, ProviderError> {
        use super::dialect::{OutputFraming, PromptChannel};

        let label = cli_label(&inv.executable);

        let argv = self.dialect.argv(&inv);
        let state_dirs = self.dialect.state_dirs();
        let mut cmd = build_sandboxed_command(
            &inv.executable,
            &argv,
            self.dialect.env(),
            &inv.stripped_env,
            inv.cwd.as_deref(),
            &inv.sandbox,
            &state_dirs,
        )?;

        // A prompt not sent on stdin is already in the argv.
        let feed_stdin = self.dialect.prompt_channel() == PromptChannel::Stdin;
        let argv = ChildIo::argv_of(cmd.as_std());
        cmd.stdin(if feed_stdin {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

        // Its own process group, so the run's end and the signal-time reaper kill the helpers the
        // CLI forks along with it.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn().map_err(|e| {
            // Under a jail the program run is the wrapper, not the CLI.
            let program = cmd.as_std().get_program().to_string_lossy().into_owned();
            spawn_error(&program, &inv.executable, e)
        })?;
        ex.argv = Some(argv);

        let group = child.id();
        let _reaper_guard = group.map(ReaperGuard::new);

        if feed_stdin {
            let mut stdin = child.stdin.take().ok_or_else(|| {
                ProviderError::Internal(format!("{label} child has no stdin pipe (piped above)"))
            })?;
            write_stdin(&mut stdin, inv.prompt.as_bytes(), ex)
                .await
                .map_err(|e| {
                    cli_subprocess_died(
                        None,
                        format!(
                            "stdin write failed after {} of {} bytes: {e}",
                            ex.stdin.len(),
                            inv.prompt.len()
                        ),
                    )
                })?;
            drop(stdin);
        }

        // Both pipes are read while waiting, or a full pipe would block the child.
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let end = {
            let mut stdout_pipe = child.stdout.take();
            let mut stderr_pipe = child.stderr.take();
            let reads = async {
                tokio::join!(
                    read_pipe(stdout_pipe.as_mut(), &mut stdout_buf),
                    read_pipe(stderr_pipe.as_mut(), &mut stderr_buf)
                )
            };
            wait_with_reads(&mut child, reads, inv.timeout).await
        };
        ex.stdout = stdout_buf.into();
        ex.stderr = stderr_buf.into();
        let status = match end {
            ChildEnd::TimedOut => {
                end_child(&mut child, group, true).await;
                return Err(ProviderError::TimedOut {
                    budget: inv.timeout,
                    detail: format!(
                        "{label} killed after wall-clock timeout (model={})",
                        inv.model
                    ),
                });
            }
            ChildEnd::Exited { status, reads } => {
                end_child(&mut child, group, false).await;
                let status =
                    status.map_err(|e| cli_subprocess_died(None, format!("wait failed: {e}")))?;
                if let Some((out, err)) = reads {
                    out.and(err).map_err(|e| {
                        cli_subprocess_died(
                            status.code(),
                            format!("reading {label}'s output failed: {e}"),
                        )
                    })?;
                }
                status
            }
        };

        if !status.success() {
            let stderr_text = String::from_utf8_lossy(&ex.stderr);
            let truncated = truncate(&stderr_text, 500);
            return Err(cli_subprocess_died(
                status.code(),
                format!("{label} exited non-zero: {truncated}"),
            ));
        }

        let stdout = String::from_utf8_lossy(&ex.stdout);
        match self.dialect.output_framing() {
            OutputFraming::SingleObject { strip_prefix } => {
                // No dialect sets `strip_prefix` today.
                let json_text = if strip_prefix {
                    &stdout[stdout.find('{').unwrap_or(0)..]
                } else {
                    stdout.as_ref()
                };
                stdout_object("CLI stdout", json_text, &stdout)
            }
            OutputFraming::JsonLinesArray => {
                // Each line stays a string; the dialect's `parse_line` decides which may fail.
                let lines: Vec<Value> = stdout
                    .lines()
                    .map(|l| Value::String(l.to_string()))
                    .collect();
                Ok(Value::Array(lines))
            }
            OutputFraming::RawText => Ok(Value::String(stdout.into_owned())),
        }
    }
}

/// Returns the reply in `.result`, or "" when it is absent or not a string.
pub(crate) fn extract_result_text(payload: &Value) -> String {
    payload
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Returns the error a claude `result` object reports with `is_error`: its `result` text, or the
/// whole object when it has none. `None` when it reports no error.
pub(super) fn reported_error(result: &Value) -> Option<String> {
    if !result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    Some(
        result
            .get("result")
            .and_then(Value::as_str)
            .map_or_else(|| result.to_string(), str::to_string),
    )
}

pub(crate) fn extract_usage(payload: &Value) -> Usage {
    let usage = match payload.get("usage").and_then(|u| u.as_object()) {
        Some(u) => u,
        None => return Usage::default(),
    };
    let field = |name: &str| usage.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
    // claude's `input_tokens` excludes the cached and cache-creation tokens, which tars's
    // `input_tokens` includes.
    let fresh = field("input_tokens");
    let cached = field("cache_read_input_tokens");
    let created = field("cache_creation_input_tokens");
    Usage {
        input_tokens: fresh.saturating_add(cached).saturating_add(created),
        output_tokens: field("output_tokens"),
        cached_input_tokens: cached,
        cache_creation_tokens: created,
        thinking_tokens: 0,
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    let trimmed = crate::provider::http_base::truncate_utf8(s, max);
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
    use tars_sandbox::SandboxPolicy;

    // Unfolded, these numbers made `Pricing::cost_for` panic in a debug build.
    #[test]
    fn an_error_result_without_text_carries_the_whole_result() {
        let result =
            serde_json::json!({"type": "result", "is_error": true, "subtype": "error_max_turns"});
        let detail = reported_error(&result).unwrap();
        assert!(detail.contains("error_max_turns"), "{detail}");
        assert_eq!(
            reported_error(&serde_json::json!({"is_error": true, "result": "quota"})).as_deref(),
            Some("quota")
        );
        assert_eq!(
            reported_error(&serde_json::json!({"is_error": false})),
            None
        );
    }

    #[test]
    fn usage_counts_past_u64_saturate_instead_of_wrapping() {
        let payload = serde_json::json!({"usage": {
            "input_tokens": u64::MAX,
            "cache_read_input_tokens": 5,
            "cache_creation_input_tokens": 7,
        }});
        assert_eq!(extract_usage(&payload).input_tokens, u64::MAX);
    }

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

        assert_eq!(usage.input_tokens, 10 + 23808 + 100);
        assert_eq!(usage.cached_input_tokens, 23808);
        assert_eq!(usage.cache_creation_tokens, 100);
        assert_eq!(usage.output_tokens, 42);

        assert!(usage.cached_input_tokens + usage.cache_creation_tokens <= usage.input_tokens);

        let pricing = Pricing {
            input_per_million: tars_types::Rate::new(3.0).unwrap(),
            output_per_million: tars_types::Rate::new(15.0).unwrap(),
            cached_input_per_million: tars_types::Rate::new(0.3).unwrap(),
            cache_creation_per_million: tars_types::Rate::new(3.75).unwrap(),
            thinking_per_million: tars_types::Rate::new(0.0).unwrap(),
        };
        let cost = pricing.cost_for(&usage);
        let expected =
            10.0 * 3.0 / 1e6 + 42.0 * 15.0 / 1e6 + 23808.0 * 0.3 / 1e6 + 100.0 * 3.75 / 1e6;
        assert!(
            (cost.0 - expected).abs() < 1e-12,
            "cost {} != {expected}",
            cost.0
        );
    }

    #[test]
    fn a_variable_reaches_a_cli_only_when_it_is_a_system_one_or_needed() {
        let none = HashSet::new();
        assert!(passes_env("PATH", &[], &none));
        assert!(passes_env("lc_all", &[], &none));
        assert!(passes_env("https_proxy", &[], &none));
        assert!(!passes_env("GEMINI_API_KEY", &[], &none));
        assert!(passes_env("Gemini_Api_Key", &["GEMINI_API_KEY"], &none));
    }

    #[test]
    fn a_stripped_variable_never_reaches_a_cli_even_when_needed() {
        let stripped = HashSet::from(["ANTHROPIC_API_KEY".to_string()]);
        assert!(!passes_env(
            "anthropic_api_key",
            &["ANTHROPIC_API_KEY"],
            &stripped
        ));
    }

    #[test]
    fn explicit_workspace_write_fills_cwd_when_roots_empty() {
        let cwd = PathBuf::from("/repo/wt");
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network: true,
        };
        let (eff, root) = resolve_effective_policy(&policy, Some(&cwd)).expect("confining ⇒ Ok");
        assert_eq!(eff, SandboxPolicy::workspace_write(&cwd));
        assert_eq!(root, cwd);
    }

    #[test]
    fn explicit_workspace_write_keeps_configured_roots() {
        let cwd = PathBuf::from("/repo/wt");
        let roots = vec![PathBuf::from("/repo/wt"), PathBuf::from("/scratch")];
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: roots,
            network: false,
        };
        let (eff, _root) = resolve_effective_policy(&policy, Some(&cwd)).unwrap();
        assert_eq!(
            eff, policy,
            "configured roots and the network toggle are kept"
        );
    }

    #[test]
    fn explicit_read_only_is_honored() {
        let cwd = PathBuf::from("/repo/wt");
        let (eff, root) =
            resolve_effective_policy(&SandboxPolicy::read_only(true), Some(&cwd)).unwrap();
        assert_eq!(eff, SandboxPolicy::read_only(true));
        assert_eq!(root, cwd);
    }

    #[test]
    fn confining_policy_without_cwd_falls_back_to_process_cwd() {
        let policy = SandboxPolicy::workspace_write(&PathBuf::from("/x"));
        let (eff, root) = resolve_effective_policy(&policy, None).expect("SAFE fallback ⇒ Ok");
        assert_eq!(eff, policy);
        assert_eq!(root, std::env::current_dir().unwrap());
    }

    #[test]
    fn danger_full_access_downgrades_to_workspace_write_on_worktree() {
        let cwd = PathBuf::from("/repo/wt");
        let (eff, root) = resolve_effective_policy(&SandboxPolicy::default(), Some(&cwd)).unwrap();
        assert_eq!(eff, SandboxPolicy::workspace_write(&cwd));
        assert_eq!(root, cwd);
    }

    #[test]
    fn danger_full_access_without_cwd_confines_to_process_cwd() {
        let (eff, root) = resolve_effective_policy(&SandboxPolicy::default(), None).unwrap();
        let proc_cwd = std::env::current_dir().unwrap();
        assert_eq!(eff, SandboxPolicy::workspace_write(&proc_cwd));
        assert_eq!(root, proc_cwd);
    }

    #[test]
    fn danger_full_access_carries_network_toggle() {
        let cwd = PathBuf::from("/repo/wt");
        let no_net = SandboxPolicy::DangerFullAccess { network: false };
        let (eff, _) = resolve_effective_policy(&no_net, Some(&cwd)).unwrap();
        assert!(
            !eff.network(),
            "network=false carried through the downgrade"
        );
    }
}
