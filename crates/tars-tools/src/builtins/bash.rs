//! `bash.run`: runs `sh -c <command>` in [`ToolEnv::cwd`] under [`ToolEnv::sandbox`] and
//! returns the exit code with stdout and stderr, cut to a size cap and bounded by a timeout.
//!
//! It does not decide whether a command may run: a command can do anything, and permission
//! belongs to the agent. The command has ended when `sh` exits; a process it left running in the
//! background is not killed then, but what it writes after a short grace is not read. A call that
//! times out, is cancelled or is dropped kills every process the command started.

use std::sync::OnceLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use tars_types::JsonSchema;

use crate::ResolveRootError;
use crate::tool::{Tool, ToolEnv, ToolError, ToolOutput};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Caps, in bytes, the text the model sees and what is kept of each output stream while the
/// command runs; the bytes before the kept tail are counted and dropped as they arrive.
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
/// How long the output pipes are still read after `sh` exits. A process the command left running
/// (`server &`) can hold them open for ever, while the command's own output was written before
/// `sh` exited.
const PIPE_GRACE: Duration = Duration::from_secs(1);

pub struct BashTool {
    timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Sets the per-call timeout; a command that runs past it is reported as a failed operation.
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns `s` cut to its last `max` bytes at a character boundary, after a note counting what was
/// cut and the `dropped_before` bytes that came before `s` and were never kept.
fn truncate_tail(s: &str, max: usize, dropped_before: u64) -> String {
    // The tail is kept because errors and test failures land at the end.
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    let cut = dropped_before + start as u64;
    if cut == 0 {
        return s.to_string();
    }
    format!("…[{cut} bytes truncated]…\n{}", &s[start..])
}

/// Returns how many bytes to drop from the front of a buffer of `len` bytes that keeps a tail of
/// `cap`: none until it holds twice the cap, so the buffer is moved once per `cap` bytes read.
fn excess(len: usize, cap: usize) -> usize {
    if len > 2 * cap { len - cap } else { 0 }
}

/// The last bytes one output stream wrote, at most [`MAX_OUTPUT_BYTES`] once finished, and how
/// many bytes came before them.
#[derive(Debug, Default)]
struct Tail {
    bytes: Vec<u8>,
    dropped: u64,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        self.drop_front(excess(self.bytes.len(), MAX_OUTPUT_BYTES));
    }

    fn drop_front(&mut self, n: usize) {
        self.bytes.drain(..n);
        self.dropped += n as u64;
    }

    /// Returns the kept text and the count of bytes before it. A tail that was cut starts at the
    /// next whole character, so a cut never shows as a replacement character.
    fn finish(mut self) -> (String, u64) {
        self.drop_front(self.bytes.len().saturating_sub(MAX_OUTPUT_BYTES));
        if self.dropped > 0 {
            let partial = self
                .bytes
                .iter()
                .take_while(|b| (0x80..0xC0).contains(*b))
                .count();
            self.drop_front(partial);
        }
        (
            String::from_utf8_lossy(&self.bytes).into_owned(),
            self.dropped,
        )
    }
}

/// Reads `pipe` to its end into `tail`; an absent pipe reads nothing.
async fn drain<R>(pipe: Option<R>, tail: &mut Tail) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf).await? {
            0 => return Ok(()),
            n => tail.push(&buf[..n]),
        }
    }
}

/// How the command's run ended, as [`wait_with_reads`] saw it.
enum RunEnd<T> {
    /// `sh` exited. `reads` is what the pipe readers returned, or `None` when something still
    /// held a pipe open [`PIPE_GRACE`] after the exit.
    Exited {
        status: std::io::Result<std::process::ExitStatus>,
        reads: Option<T>,
    },
    TimedOut,
}

/// Waits for `child` to exit within `timeout` while driving `reads`, the readers of its pipes, so
/// a full pipe never blocks it. The end of the run is the exit, not the pipes' end-of-file: after
/// it `reads` gets [`PIPE_GRACE`] more and is then dropped.
async fn wait_with_reads<F: std::future::Future>(
    child: &mut tokio::process::Child,
    reads: F,
    timeout: Duration,
) -> RunEnd<F::Output> {
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::pin!(reads);
    let mut done = None;
    let status = loop {
        tokio::select! {
            // `Child::wait` is cancel-safe, so a new one each turn loses nothing.
            status = child.wait() => break status,
            out = &mut reads, if done.is_none() => done = Some(out),
            () = tokio::time::sleep_until(deadline) => return RunEnd::TimedOut,
        }
    };
    let reads = match done {
        Some(out) => Some(out),
        None => tokio::time::timeout(PIPE_GRACE, reads).await.ok(),
    };
    RunEnd::Exited { status, reads }
}

/// The process group a spawned `sh` leads. Dropping it kills the group, so a call that times out,
/// is cancelled or is dropped leaves nothing the command started; [`ProcessGroup::release`] keeps
/// it once `sh` has exited on its own.
struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    fn release(mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }
}

/// SIGKILLs, on unix, the process group led by `pid`; a group with no process left is not an
/// error. Elsewhere nothing is killed, as there is no group.
fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        let Some(group) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
            tracing::warn!(pid, "bash.run: the PID names no process group");
            return;
        };
        match kill_process_group(group, Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(e) => tracing::warn!(pid, error = %e, "bash.run: killing the process group failed"),
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Returns the note that names each writable root left out of the jail, so a write the jail
/// then denies is not a mystery to the model; empty when none was.
fn dropped_note(dropped: &[ResolveRootError]) -> String {
    dropped
        .iter()
        .map(|d| {
            format!(
                "\n(writable root `{}` cannot be resolved and was left out of the sandbox: {})",
                d.root.display(),
                d.source
            )
        })
        .collect()
}

/// Returns the note for output the run could not read whole: a pipe a process left behind still
/// held open after `sh` exited, or a read that failed. Empty when both pipes were read to their
/// end.
fn unread_note(reads: Option<(std::io::Result<()>, std::io::Result<()>)>) -> String {
    let Some((stdout, stderr)) = reads else {
        return "\n(a process the command started still holds its output open; what it writes \
                after the command exited is not shown)"
            .to_string();
    };
    [("stdout", stdout), ("stderr", stderr)]
        .into_iter()
        .filter_map(|(name, read)| {
            read.err()
                .map(|e| format!("\n(reading {name} failed: {e})"))
        })
        .collect()
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash.run"
    }

    fn description(&self) -> &str {
        "Run a shell command (via `sh -c`) in the working directory and \
         return its exit code, stdout and stderr, each in its own section. Use for build, \
         test, git, and other actions on the tree. To SEARCH code, prefer \
         fs.grep (contents) and fs.glob (file names) — they are faster and \
         scoped to the workspace; avoid `grep`/`find` here."
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "BashArgs",
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "command": { "type": "string", "description": "Shell command line, run via `sh -c`." }
                    },
                    "required": ["command"]
                },
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        env: &ToolEnv,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let parsed: BashArgs = serde_json::from_value(args).map_err(ToolError::InvalidArguments)?;

        // A sandbox that cannot be built fails the call; the command never runs unconfined.
        let workdir = env
            .cwd
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let wrapped = env
            .sandbox
            .wrap("sh", &["-c".to_string(), parsed.command.clone()], &workdir)
            .map_err(ToolError::Sandbox)?;
        let dropped = dropped_note(&wrapped.dropped);
        let mut cmd = tokio::process::Command::new(wrapped.program);
        cmd.args(wrapped.args);
        if let Some(cwd) = env.cwd.as_deref() {
            cmd.current_dir(cwd);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn().map_err(ToolError::Spawn)?;
        let group = ProcessGroup(child.id());
        let (stdout_pipe, stderr_pipe) = (child.stdout.take(), child.stderr.take());
        let (mut stdout, mut stderr) = (Tail::default(), Tail::default());
        let end = {
            let reads = async {
                tokio::join!(
                    drain(stdout_pipe, &mut stdout),
                    drain(stderr_pipe, &mut stderr)
                )
            };
            tokio::select! {
                _ = cancel.cancelled() => return Err(ToolError::Cancelled),
                end = wait_with_reads(&mut child, reads, self.timeout) => end,
            }
        };
        let (status, reads) = match end {
            RunEnd::TimedOut => {
                drop(group);
                if let Err(e) = child.wait().await {
                    tracing::debug!(error = %e, "bash.run: reaping after the timeout failed");
                }
                return Ok(ToolOutput::error(format!(
                    "command timed out after {}s: {}",
                    self.timeout.as_secs(),
                    truncate_tail(&parsed.command, 120, 0)
                )));
            }
            RunEnd::Exited { status, reads } => {
                group.release();
                (status.map_err(ToolError::Spawn)?, reads)
            }
        };

        let code = status.code();
        let (stdout, stdout_dropped) = stdout.finish();
        let (stderr, stderr_dropped) = stderr.finish();
        let combined = format!(
            "exit: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            stdout.trim_end(),
            stderr.trim_end(),
        );
        let body = truncate_tail(&combined, MAX_OUTPUT_BYTES, stdout_dropped + stderr_dropped)
            + &unread_note(reads)
            + &dropped;
        // A non-zero exit is not `is_error`: `grep` with no match exits 1, and the session aborts
        // after a run of turns whose every call is an error, so a few empty greps would end a
        // working agent. The exit code is in the body and the title.
        let title = match code {
            Some(0) => "ran command".to_string(),
            Some(c) => format!("ran command (exit {c})"),
            None => "ran command (killed by signal)".to_string(),
        };
        Ok(ToolOutput::titled_success(title, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    fn ctx(cwd: Option<PathBuf>) -> ToolEnv {
        ToolEnv {
            cwd,
            ..Default::default()
        }
    }

    #[test]
    fn only_a_fork_refused_for_want_of_processes_may_pass_on_another_try() {
        let eagain = std::io::Error::from(std::io::ErrorKind::WouldBlock);
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);

        assert!(ToolError::Spawn(eagain).is_transient());
        assert!(!ToolError::Spawn(missing).is_transient());
    }

    #[tokio::test]
    async fn runs_in_cwd_and_reports_exit_zero() {
        let dir = std::env::temp_dir().join(format!("tars_bash_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), "x").unwrap();
        let r = BashTool::new()
            .execute(
                json!({ "command": "ls marker.txt" }),
                &ctx(Some(dir.clone())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("marker.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_confines_bash_writes_to_worktree() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let wt = base.join(format!("tars_bash_jail_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt);
        std::fs::create_dir_all(&wt).unwrap();
        let wt = std::fs::canonicalize(&wt).unwrap();
        let outside = base.join(format!("tars_bash_escape_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&outside);

        let jailed = || {
            let mut c = ctx(Some(wt.clone()));
            c.sandbox = crate::SandboxPolicy::workspace_write(&wt);
            c
        };

        BashTool::new()
            .execute(
                json!({ "command": "echo ok > inside.txt" }),
                &jailed(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            wt.join("inside.txt").exists(),
            "write inside worktree must succeed"
        );

        let _ = BashTool::new()
            .execute(
                json!({ "command": format!("echo pwned > {}", outside.display()) }),
                &jailed(),
                CancellationToken::new(),
            )
            .await;
        assert!(
            !outside.exists(),
            "sandbox MUST block bash writes outside the worktree"
        );

        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_file(&outside);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn without_a_cwd_the_writable_roots_git_is_still_protected() {
        let dir = tempfile::tempdir().unwrap();
        let wt = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(wt.join(".git/hooks")).unwrap();
        let hook = wt.join(".git/hooks/pre-commit");
        let mut c = ctx(None);
        c.sandbox = crate::SandboxPolicy::workspace_write(&wt);
        BashTool::new()
            .execute(
                json!({ "command": format!("echo pwned > {}", hook.display()) }),
                &c,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!hook.exists(), "the writable root's hooks were written");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_directory_holding_a_protected_git_cannot_be_renamed() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(repo.join("pkg")).unwrap();
        let mut c = ctx(Some(repo.join("pkg")));
        c.sandbox = crate::SandboxPolicy::workspace_write(&base);
        let command = "cd ../.. && mv repo repo2 && echo pwned > repo2/.git/hooks/pre-commit; \
                       mv repo2 repo";
        BashTool::new()
            .execute(json!({ "command": command }), &c, CancellationToken::new())
            .await
            .unwrap();
        assert!(repo.exists(), "the repository was moved");
        assert!(
            !repo.join(".git/hooks/pre-commit").exists(),
            "the hook was written"
        );
    }

    #[test]
    fn a_jail_that_dropped_no_root_adds_no_note() {
        assert_eq!(dropped_note(&[]), "");
    }

    #[test]
    fn a_dropped_writable_root_is_named() {
        let note = dropped_note(&[ResolveRootError {
            root: PathBuf::from("/deps/not-fetched"),
            source: std::io::ErrorKind::NotFound.into(),
        }]);
        assert!(
            note.contains("`/deps/not-fetched` cannot be resolved"),
            "{note}"
        );
    }

    #[test]
    fn a_tail_cut_counts_the_bytes_it_never_kept() {
        assert_eq!(truncate_tail("abcdef", 6, 0), "abcdef");
        assert_eq!(truncate_tail("abcdef", 4, 0), "…[2 bytes truncated]…\ncdef");
        assert_eq!(truncate_tail("abc", 4, 10), "…[10 bytes truncated]…\nabc");
    }

    #[test]
    fn a_tail_holds_at_most_twice_the_cap_while_reading_and_the_cap_when_done() {
        let mut tail = Tail::default();
        for _ in 0..100 {
            tail.push(&[b'x'; 4096]);
            assert!(tail.bytes.len() <= 2 * MAX_OUTPUT_BYTES + 4096);
        }
        let (text, dropped) = tail.finish();
        assert_eq!(text.len(), MAX_OUTPUT_BYTES);
        assert_eq!(dropped, 100 * 4096 - MAX_OUTPUT_BYTES as u64);
    }

    #[test]
    fn a_cut_tail_starts_at_a_whole_character() {
        let mut tail = Tail::default();
        tail.push("é".repeat(MAX_OUTPUT_BYTES).as_bytes());
        tail.push(b"x");
        let (text, dropped) = tail.finish();
        assert!(!text.contains('\u{FFFD}'), "{:?}", &text[..8]);
        assert_eq!(text.len() as u64 + dropped, 2 * MAX_OUTPUT_BYTES as u64 + 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_ends_when_sh_exits_though_its_background_process_holds_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let started = std::time::Instant::now();
        let r = BashTool::new()
            .timeout(Duration::from_secs(5))
            .execute(
                json!({ "command": format!("sleep 30 & echo $! > {}; echo finished", pidfile.display()) }),
                &ctx(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        std::process::Command::new("kill")
            .arg(pid.trim())
            .status()
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("exit: 0") && r.content.contains("finished"),
            "{}",
            r.content
        );
        assert!(
            r.content.contains("still holds its output open"),
            "{}",
            r.content
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "took {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_command_takes_its_background_processes_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let r = BashTool::new()
            .timeout(Duration::from_secs(1))
            .execute(
                json!({ "command": format!("sleep 30 & echo $! > {}; wait", pidfile.display()) }),
                &ctx(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            r.is_error && r.content.contains("timed out"),
            "{}",
            r.content
        );
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        let alive = || {
            std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let survived = alive();
        if survived {
            std::process::Command::new("kill")
                .arg(pid.trim())
                .status()
                .unwrap();
        }
        assert!(
            !survived,
            "the background `sleep` outlived the timed-out call"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported_in_body_not_flagged_as_tool_error() {
        let r = BashTool::new()
            .execute(
                json!({ "command": "exit 3" }),
                &ctx(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "non-zero exit is data, not a tool error");
        assert!(r.content.contains("exit: 3"), "exit code stays in the body");
    }
}
