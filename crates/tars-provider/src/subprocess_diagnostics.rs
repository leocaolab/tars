//! Subprocess exit diagnostics — the "uncaught-exception printer" for
//! child processes the way `#[tracing::instrument(err)]` is for
//! `Result::Err`.
//!
//! When tars or arc spawn a child (claude-cli, codex, gemini-cli, …)
//! and that child exits non-zero, the operator deserves more than a
//! bare `i32`. We saw this the hard way: an `arc auto` run died with
//! exit 145 and it took 30 minutes of grepping claude-code source +
//! the JSONL session transcript + macOS system log to figure out the
//! 145 meant "Bash tool aborted before execution" (claude-code's
//! `ShellCommand.ts:420` default).
//!
//! This module provides one entry point, [`diagnose_child_exit`],
//! that returns a [`SubprocessDiagnostics`] value the caller prints
//! verbatim. It does NOT panic, does NOT take ownership of the child,
//! and is safe to call repeatedly. It assumes nothing about the
//! caller's tracing setup — outputs are plain text suitable for stderr.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

/// A structured snapshot of why a child process exited and what
/// state it left behind. Display-format the whole struct for a
/// pre-formatted human dump.
#[derive(Debug)]
pub struct SubprocessDiagnostics {
    pub command: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub elapsed: Duration,
    pub interpretation: String,
    pub session_log_path: Option<PathBuf>,
    pub session_log_summary: Option<String>,
    pub worktree_diff_stat: Option<String>,
}

/// Interpret an exit code into human-readable provenance. Knows the
/// standard POSIX shell convention (128 + signal) and the claude-code
/// specific codes we've encountered in the wild.
fn interpret_exit_code(code: i32) -> &'static str {
    match code {
        0 => "success",
        1 => "general error",
        2 => "misuse of shell builtin / argv parse error",
        126 => "command found but not executable (permission denied)",
        127 => "command not found",
        128 => "invalid argument to exit",
        129 => "killed by SIGHUP",
        130 => "killed by SIGINT (Ctrl-C)",
        131 => "killed by SIGQUIT",
        134 => "killed by SIGABRT (assertion failure / Rust panic)",
        137 => "killed by SIGKILL (OOM-killer or external `kill -9`)",
        139 => "killed by SIGSEGV (segfault)",
        143 => "killed by SIGTERM (graceful termination signal)",
        // claude-code-specific exit codes we've observed:
        145 => "claude-code AbortedShellCommand default (a Bash tool was aborted before/during execution; see ShellCommand.ts:420). Common with parallel Task sub-agents stepping on each other",
        _ if code > 128 && code < 160 => "killed by signal (128 + signal_number)",
        _ => "process-specific exit code (no standard mapping)",
    }
}

/// Find the claude-code session transcript JSONL for a given working
/// directory + session id, if present. Claude-code writes per-session
/// transcripts under `~/.claude/projects/<dir-slug>/<session-id>.jsonl`
/// where `dir-slug` is the cwd path with `/` replaced by `-`.
pub fn find_claude_session_log(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let projects = PathBuf::from(home).join(".claude").join("projects");
    // claude-code's slugging: each path segment becomes its own `-`-prefixed
    // chunk, so /a/b/c → -a-b-c. Replicate that.
    let slug = {
        let mut s = String::new();
        for component in cwd.components() {
            if let std::path::Component::Normal(p) = component {
                s.push('-');
                s.push_str(&p.to_string_lossy());
            } else if matches!(component, std::path::Component::RootDir) {
                // root is implied by the leading '-' on the first Normal
            }
        }
        s
    };
    let candidate = projects.join(slug).join(format!("{session_id}.jsonl"));
    candidate.is_file().then_some(candidate)
}

/// Summarise a claude-code session transcript: how many events, did
/// the final `result` event arrive (a clean exit), and what was the
/// last tool / text? `Err` only on read failure; an absent `result`
/// event is reported in the summary string, not as an error.
pub fn summarise_claude_session_log(path: &Path) -> Result<String, std::io::Error> {
    let body = std::fs::read_to_string(path)?;
    let mut total = 0usize;
    let mut assist_calls = 0usize;
    let mut tool_results = 0usize;
    let mut saw_result_event = false;
    let mut last_ts = String::new();
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        total += 1;
        // We intentionally don't fully parse — string-grep is robust
        // against schema drift and avoids pulling serde_json just for
        // a summary. False positives on tag spelling are noise we can
        // live with.
        if line.contains("\"type\":\"result\"") {
            saw_result_event = true;
        }
        if line.contains("\"type\":\"assistant\"") {
            assist_calls += 1;
        }
        if line.contains("\"type\":\"user\"") && line.contains("tool_result") {
            tool_results += 1;
        }
        // Pull the timestamp out crudely.
        if let Some(idx) = line.find("\"timestamp\":\"") {
            let start = idx + "\"timestamp\":\"".len();
            if let Some(end_rel) = line[start..].find('"') {
                last_ts = line[start..start + end_rel].to_string();
            }
        }
    }
    let mid_stream = if saw_result_event {
        "result event present (clean exit)"
    } else {
        "NO `result` event → process was killed mid-stream"
    };
    Ok(format!(
        "{total} events ({assist_calls} assistant turns, {tool_results} tool_results); \
         last event ts={last_ts}; {mid_stream}"
    ))
}

/// `git diff --stat` on the worktree, single-line summary. Best-effort:
/// returns `None` if `git` fails (e.g. not a repo).
pub fn worktree_diff_summary(repo: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["diff", "--shortstat", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Main entry point — given an `ExitStatus` and the context the child
/// was launched in, produce a [`SubprocessDiagnostics`] suitable for
/// printing to stderr.
///
/// `claude_session_id` is optional — pass `Some` only when the child
/// was a claude-code CLI invocation; it scopes the session-log
/// lookup. Other subprocesses pass `None` and skip that block.
pub fn diagnose_child_exit(
    command: impl Into<String>,
    status: ExitStatus,
    elapsed: Duration,
    repo: &Path,
    claude_session_id: Option<&str>,
) -> SubprocessDiagnostics {
    use std::os::unix::process::ExitStatusExt;
    let exit_code = status.code();
    let signal = status.signal();
    let interpretation = match (exit_code, signal) {
        (Some(c), _) => format!("exit {c}: {}", interpret_exit_code(c)),
        (None, Some(s)) => format!("killed by signal {s}"),
        (None, None) => "unknown termination (no exit code, no signal)".to_string(),
    };
    let (session_log_path, session_log_summary) =
        if let Some(sid) = claude_session_id {
            if let Some(p) = find_claude_session_log(repo, sid) {
                let summary = summarise_claude_session_log(&p)
                    .unwrap_or_else(|e| format!("(failed to read: {e})"));
                (Some(p), Some(summary))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
    let worktree_diff_stat = worktree_diff_summary(repo);
    SubprocessDiagnostics {
        command: command.into(),
        exit_code,
        signal,
        elapsed,
        interpretation,
        session_log_path,
        session_log_summary,
        worktree_diff_stat,
    }
}

impl std::fmt::Display for SubprocessDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "── subprocess diagnostics ──────────────────────────────────",
        )?;
        writeln!(f, "  command:        {}", self.command)?;
        writeln!(f, "  elapsed:        {:?}", self.elapsed)?;
        writeln!(f, "  meaning:        {}", self.interpretation)?;
        if let Some(path) = &self.session_log_path {
            writeln!(f, "  session log:    {}", path.display())?;
        }
        if let Some(summary) = &self.session_log_summary {
            writeln!(f, "  session state:  {summary}")?;
        }
        if let Some(diff) = &self.worktree_diff_stat {
            writeln!(f, "  worktree diff:  {diff}")?;
        }
        writeln!(
            f,
            "────────────────────────────────────────────────────────────",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn interpret_exit_codes_covers_the_signals_we_care_about() {
        for (code, want_substr) in [
            (0, "success"),
            (1, "general error"),
            (127, "not found"),
            (137, "SIGKILL"),
            (143, "SIGTERM"),
            (145, "AbortedShellCommand"),
            (250, "process-specific"),
        ] {
            let got = interpret_exit_code(code);
            assert!(
                got.contains(want_substr),
                "exit code {code} should mention {want_substr:?}, got {got:?}",
            );
        }
    }

    #[test]
    fn diagnose_displays_known_meaning_for_145() {
        let status = ExitStatus::from_raw(145 << 8); // wait()'s status format: exit code in high byte
        let dummy_repo = std::env::temp_dir();
        let diag = diagnose_child_exit(
            "claude -p -",
            status,
            Duration::from_secs(360),
            &dummy_repo,
            None, // no session id
        );
        let rendered = format!("{diag}");
        assert!(rendered.contains("145"), "code in output: {rendered}");
        assert!(rendered.contains("AbortedShellCommand"), "meaning in output: {rendered}");
        assert!(rendered.contains("claude -p -"), "command in output: {rendered}");
    }

    #[test]
    fn find_session_log_returns_none_when_absent() {
        // No HOME pollution, no real session expected.
        let result = find_claude_session_log(
            Path::new("/tmp/definitely/does/not/exist"),
            "fake-session-id-00000000",
        );
        assert!(result.is_none(), "non-existent log: {result:?}");
    }

    #[test]
    fn summarise_session_log_handles_missing_result_event() {
        // Hand-build a minimal jsonl in tmp that mimics the
        // 39-event-but-no-result-event shape we saw in the
        // killed-mid-stream incident.
        let tmpdir = std::env::temp_dir().join(format!(
            "tars_subprocess_diag_test_{}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&tmpdir).unwrap();
        let log_path = tmpdir.join("session.jsonl");
        let body = concat!(
            "{\"type\":\"assistant\",\"timestamp\":\"2026-05-31T04:22:30Z\"}\n",
            "{\"type\":\"user\",\"timestamp\":\"2026-05-31T04:23:00Z\",\"content\":[{\"type\":\"tool_result\"}]}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-05-31T04:28:15Z\"}\n",
            // NB: no "result" event — process was killed
        );
        std::fs::write(&log_path, body).unwrap();
        let summary = summarise_claude_session_log(&log_path).unwrap();
        assert!(summary.contains("3 events"), "event count: {summary}");
        assert!(summary.contains("2 assistant turns"), "assist count: {summary}");
        assert!(summary.contains("1 tool_results"), "tool_result count: {summary}");
        assert!(summary.contains("NO `result` event"), "kill detection: {summary}");
        assert!(summary.contains("2026-05-31T04:28:15Z"), "last ts: {summary}");
        let _ = std::fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn summarise_session_log_recognises_clean_exit() {
        let tmpdir = std::env::temp_dir().join(format!(
            "tars_subprocess_diag_test_clean_{}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&tmpdir).unwrap();
        let log_path = tmpdir.join("session.jsonl");
        let body = concat!(
            "{\"type\":\"assistant\",\"timestamp\":\"2026-05-31T04:22:30Z\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"timestamp\":\"2026-05-31T04:23:00Z\"}\n",
        );
        std::fs::write(&log_path, body).unwrap();
        let summary = summarise_claude_session_log(&log_path).unwrap();
        assert!(summary.contains("result event present"), "clean exit: {summary}");
        assert!(!summary.contains("NO `result`"), "must not flag clean as killed: {summary}");
        let _ = std::fs::remove_dir_all(&tmpdir);
    }
}
