//! Delegate-CLI security integration test (M5b/M6b, path = `claude_cli`).
//!
//! A mock "claude"-like CLI binary is driven through the **real production
//! subprocess path** ([`RealSubprocessRunner`]) with the OS write-jail on
//! (`TARS_CLAUDE_SANDBOX=1`), and we assert the escape is blocked while normal
//! operation (JSON round-trip, worktree write) still works. This proves the
//! WIRING (the real `run` path actually applies `tars_sandbox`) together with
//! CONTAINMENT — the "synthesized mock agent through the real path" the design
//! calls for (tracking doc §3 fixer/merge row, M6(b); guardrails 2/5/6).
//!
//! We deliberately do NOT call `SandboxPolicy::wrap` directly — the whole point
//! is to exercise `RealSubprocessRunner::run`, the production seam.
//!
//! Runs on macOS (Seatbelt via `/usr/bin/sandbox-exec`) and Linux (bubblewrap).
//!
//! ## Why this test re-execs itself
//! The runner gates the sandbox on the process env var `TARS_CLAUDE_SANDBOX`,
//! read from `RealSubprocessRunner`'s OWN process. The workspace forbids
//! `unsafe`, and `std::env::set_var` is `unsafe` in Rust 2024, so the test
//! cannot mutate its own env in-process. Instead the test re-execs its own test
//! binary with the env var set on the child `Command` (which IS safe). The
//! re-exec'd child (marked by [`CHILD_MARKER`]) runs the real assertions; the
//! env stays isolated to that one child process (guardrail: no global mutation).

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::Duration;

use tars_provider::backends::claude_cli::{
    ClaudeCliTools, RealSubprocessRunner, SubprocessInvocation, SubprocessRunner,
};

/// Set on the re-exec'd child so it runs the real body instead of re-exec'ing
/// again (guards against infinite recursion).
const CHILD_MARKER: &str = "TARS_SANDBOX_IT_CHILD";
const TEST_NAME: &str = "delegate_cli_escape_blocked_through_real_run_path";

#[test]
fn delegate_cli_escape_blocked_through_real_run_path() {
    if std::env::var(CHILD_MARKER).is_ok() {
        // We ARE the sandboxed child (env set by the parent's re-exec below):
        // run the real assertions against `RealSubprocessRunner`.
        run_child_body();
        return;
    }

    // Parent: re-exec THIS test binary, running only this one test, with the
    // sandbox flag set in the child's env. `Command::env` is safe (unlike
    // `env::set_var`), and the mutation is isolated to the child process.
    let exe = std::env::current_exe().expect("locate current test binary");
    let status = StdCommand::new(exe)
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env("TARS_CLAUDE_SANDBOX", "1")
        .env(CHILD_MARKER, "1")
        // Keep the child deterministic/offline: force buffered json parsing so a
        // stray inherited streaming flag can't change the parse shape.
        .env_remove("TARS_CLAUDE_CLI_STREAM")
        .status()
        .expect("re-exec sandboxed test child");
    assert!(
        status.success(),
        "sandboxed delegate-CLI child assertions failed (output above)"
    );
}

/// The real test, run inside the re-exec'd child where `TARS_CLAUDE_SANDBOX=1`.
fn run_child_body() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async { escape_blocked_async().await });
}

async fn escape_blocked_async() {
    let tag = format!("tars_delegate_{}", std::process::id());

    // ── worktree: the ONLY place the delegate may write ──────────────────
    let worktree = fresh_dir(&format!("{tag}_wt"));
    let inside_file = worktree.join("inside.txt");

    // ── outside area: canonical dir OUTSIDE the delegate's writable set that
    //    it must not be able to touch. It lives under $HOME (see
    //    `fresh_denied_dir`), NOT under $TMPDIR: the codex-model jail now grants
    //    the real $TMPDIR + /tmp as writable, so the escape target must be
    //    somewhere still denied. Canonicalized so the absolute paths are real
    //    paths (macOS `/tmp`→`/private/tmp`), matching the Seatbelt jail's view. ──
    let outside_dir = fresh_denied_dir(&format!("{tag}_outside"));
    let outside_create = outside_dir.join("escaped.txt"); // delegate tries to CREATE → must fail
    let outside_victim = outside_dir.join("victim.txt"); // pre-existing → delegate tries to rm
    std::fs::write(&outside_victim, b"i must survive").expect("seed outside victim");

    // ── mock "claude" CLI: a chmod+x shell script that (a) drains stdin,
    //    (b) ATTEMPTS to escape the worktree (create + delete outside), (c)
    //    writes a legit file INSIDE the worktree, then (d) prints claude-CLI-
    //    shaped JSON so `RealSubprocessRunner::run` parses it. The escape
    //    attempts are `|| :`-guarded so the script still exits 0 and emits its
    //    JSON even when the jail denies the writes (EPERM). ──
    let script = mock_cli_script(&outside_create, &outside_victim, &inside_file);
    // The mock lives INSIDE the worktree so the exec'd binary is reachable under
    // BOTH jails: macOS Seatbelt exposes the whole fs (read is broad, so anywhere
    // works), but Linux bubblewrap is a mount namespace whose `--tmpfs /tmp` masks
    // the real `/tmp` — a script placed in the `_outside` dir (under `/tmp`) would
    // be invisible and fail `execvp`. The worktree is bind-mounted into both jails,
    // so it is the one location the binary is guaranteed to exec from. This does not
    // weaken the escape test: the escape TARGETS (`outside_create`/`outside_victim`)
    // stay outside the worktree.
    let script_path = worktree.join("mock_claude.sh");
    std::fs::write(&script_path, script).expect("write mock CLI");
    make_executable(&script_path);

    // ── build the invocation through the SAME struct production uses, with
    //    cwd = worktree so the runner wraps the spawn in the write-jail. ──
    let inv = SubprocessInvocation {
        executable: script_path.to_string_lossy().into_owned(),
        model: "mock-model".into(),
        system: None,
        prompt: "please escape the worktree".into(),
        timeout: Duration::from_secs(30),
        stripped_env: HashSet::new(),
        tools: ClaudeCliTools::default(),
        bare: false,
        effort: None,
        exclude_dynamic_sections: false,
        extra_args: Vec::new(),
        cwd: Some(worktree.clone()),
        // Unconfined policy: this test drives the LEGACY `TARS_CLAUDE_SANDBOX=1`
        // env-gate path (set on the re-exec'd child), which must keep working
        // for back-compat. `DangerFullAccess` here proves the env gate still
        // jails the spawn even when no explicit `[sandbox]`/`--sandbox` policy
        // is set (the G10 policy path is exercised by provider unit tests).
        sandbox: tars_sandbox::SandboxPolicy::default(),
    };

    // ── drive the REAL production path ───────────────────────────────────
    let result = RealSubprocessRunner.run(inv).await;

    // 1. Normal operation survived the sandbox: the mock's JSON round-trips.
    let payload = result.expect("RealSubprocessRunner.run should succeed (mock JSON parsed)");
    assert_eq!(
        payload.get("type").and_then(|v| v.as_str()),
        Some("result"),
        "mock CLI JSON did not round-trip through the real run path: {payload:?}"
    );
    assert_eq!(
        payload.get("result").and_then(|v| v.as_str()),
        Some("done"),
        "mock CLI result field did not round-trip: {payload:?}"
    );

    // 2. Worktree write ALLOWED: sandbox didn't break legitimate operation.
    assert!(
        inside_file.exists(),
        "delegate should be able to write inside the worktree ({})",
        inside_file.display()
    );

    // 3. Escape BLOCKED — create outside the worktree must have failed.
    assert!(
        !outside_create.exists(),
        "SANDBOX ESCAPE: delegate created a file OUTSIDE the worktree ({})",
        outside_create.display()
    );

    // 4. Escape BLOCKED — delete of a pre-existing outside file must have failed.
    assert!(
        outside_victim.exists(),
        "SANDBOX ESCAPE: delegate deleted a file OUTSIDE the worktree ({})",
        outside_victim.display()
    );
    assert_eq!(
        std::fs::read(&outside_victim).expect("read victim"),
        b"i must survive",
        "outside victim file was tampered with"
    );

    // cleanup (best-effort)
    let _ = std::fs::remove_dir_all(&worktree);
    let _ = std::fs::remove_dir_all(&outside_dir);
}

/// Build the mock CLI script body. Absolute paths are baked in so the child
/// does not depend on cwd/TMPDIR.
fn mock_cli_script(outside_create: &Path, outside_victim: &Path, inside_file: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # (a) drain stdin (the prompt) so the parent's stdin write completes\n\
         cat > /dev/null\n\
         # (b) ATTEMPT escape: create a file outside the worktree (jail must deny)\n\
         echo pwned > '{create}' 2>/dev/null || :\n\
         # (b) ATTEMPT escape: delete a pre-existing outside file (jail must deny)\n\
         rm -f '{victim}' 2>/dev/null || :\n\
         # (c) legit: write inside the worktree (jail must allow)\n\
         echo done > '{inside}'\n\
         # (d) emit claude-CLI-shaped JSON on stdout for the runner to parse\n\
         printf '{{\"type\":\"result\",\"result\":\"done\",\"is_error\":false}}\\n'\n",
        create = outside_create.display(),
        victim = outside_victim.display(),
        inside = inside_file.display(),
    )
}

/// Create a fresh directory under the temp dir and return its CANONICAL path
/// (macOS `/tmp`→`/private/tmp`, so it matches the Seatbelt jail's real-path view).
/// Used for the worktree — which the jail DOES make writable.
fn fresh_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create dir");
    std::fs::canonicalize(&p).expect("canonicalize dir")
}

/// A fresh dir GUARANTEED outside the delegate's writable set, for the escape
/// target. The codex-model jail's writable set is: the worktree + real `$TMPDIR`
/// + `/tmp` + the CLI's own state dir (none for claude). `$HOME` at large is
/// denied, so a dir under `$HOME` is a genuine "outside" — UNLIKE `$TMPDIR`,
/// which this test used to use back when the jail denied tmp (a policy we have
/// deliberately reversed to match codex, so the target had to move here).
fn fresh_denied_dir(name: &str) -> PathBuf {
    let home = std::env::var_os("HOME").expect("HOME must be set for the escape test");
    let p = PathBuf::from(home).join(".tars-sandbox-it").join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create denied dir");
    std::fs::canonicalize(&p).expect("canonicalize denied dir")
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).expect("stat script").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod +x script");
}
