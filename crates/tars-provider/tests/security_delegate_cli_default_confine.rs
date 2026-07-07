//! E2E-4 (Doc 32 §8, CUJ-3 / FR-3): default-confine for a NON-claude delegate
//! through the **policy path**, not the legacy env gate.
//!
//! The sibling `security_delegate_cli.rs` proves the claude runner is jailed via
//! the legacy `TARS_CLAUDE_SANDBOX=1` env gate. This test proves the thing the
//! audit found missing:
//!
//!   1. a delegate OTHER than claude (here the shared `SharedCliRunner` driving a
//!      `GeminiCliDialect`, which spawns `gemini` with the prompt as an argv arg
//!      and frames a buffered single JSON object) is confined, and
//!   2. confinement comes from the **default** — the invocation carries a plain
//!      `SandboxPolicy::default()` (`DangerFullAccess`) and `TARS_CLAUDE_SANDBOX`
//!      is NOT set — so it exercises the default-confine flip
//!      (`resolve_effective_policy` downgrading `DangerFullAccess` to a
//!      workspace-write jail on the worktree cwd), NOT the env gate.
//!
//! Because no process env has to be mutated (the default confines), this test
//! runs the assertions DIRECTLY — no re-exec dance is needed (contrast the
//! env-gate test, which must set `TARS_CLAUDE_SANDBOX` on a re-exec'd child).
//!
//! macOS/Linux only (uses the real OS sandbox: Seatbelt / bubblewrap).

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tars_provider::backends::cli::{
    GeminiCliDialect, SharedCliRunner, SubprocessInvocation, SubprocessRunner,
};

#[test]
fn non_claude_delegate_default_confined_through_policy_path() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async { escape_blocked_async().await });
}

async fn escape_blocked_async() {
    let tag = format!("tars_gemini_default_{}", std::process::id());

    // ── worktree: the ONLY place the delegate may write ──────────────────
    let worktree = fresh_dir(&format!("{tag}_wt"));
    let inside_file = worktree.join("inside.txt");

    // ── outside area the delegate must not be able to touch. Under $HOME (see
    //    `fresh_denied_dir`), NOT $TMPDIR: the codex-model jail now grants the
    //    real $TMPDIR + /tmp as writable, so the escape target must be somewhere
    //    still denied. ──
    let outside_dir = fresh_denied_dir(&format!("{tag}_outside"));
    let outside_create = outside_dir.join("escaped.txt"); // delegate tries to CREATE → must fail
    let outside_victim = outside_dir.join("victim.txt"); // pre-existing → delegate tries to rm
    std::fs::write(&outside_victim, b"i must survive").expect("seed outside victim");

    // ── mock "gemini" CLI: a shell script that (a) attempts to escape the
    //    worktree (create + delete outside), (b) writes a legit file INSIDE the
    //    worktree, then (c) prints gemini-CLI-shaped JSON (`{"response":…}`) so
    //    `SharedCliRunner::run` parses it. The escape attempts are
    //    `|| :`-guarded so the script still exits 0 and emits its JSON even when
    //    the jail denies the writes (EPERM). The gemini runner passes the prompt
    //    as an argv arg (stdin is null), so the script ignores its args. ──
    let script = mock_gemini_script(&outside_create, &outside_victim, &inside_file);
    let script_path = outside_dir.join("mock_gemini.sh"); // outside worktree — reads/exec are broad
    std::fs::write(&script_path, script).expect("write mock CLI");
    make_executable(&script_path);

    // ── build the invocation with a DEFAULT policy (DangerFullAccess) and cwd =
    //    worktree. No `[sandbox]`/`--sandbox` confining policy, no
    //    TARS_CLAUDE_SANDBOX — the default-confine flip is what must jail it. ──
    let inv = SubprocessInvocation::neutral(
        script_path.to_string_lossy().into_owned(),
        "mock-gemini-model".into(),
        "please escape the worktree".into(),
        Duration::from_secs(30),
        HashSet::new(),
        Some(worktree.clone()),
        // The whole point: a plain default policy (= DangerFullAccess = today's
        // "unconfined" default) must STILL confine a black-box delegate.
        tars_sandbox::SandboxPolicy::default(),
    );

    // ── drive the REAL production runner (a NON-claude one): the shared
    //    SharedCliRunner driving a GeminiCliDialect (prompt as arg, single-object
    //    JSON framing). ─────────────────────────────────────────────────────
    let dialect = Arc::new(GeminiCliDialect::new("gemini".into(), Duration::from_secs(30)));
    let runner = SharedCliRunner::new(dialect);
    let result = runner.run(inv).await;

    // 1. Normal operation survived the sandbox: the mock's JSON round-trips.
    let payload = result.expect("SharedCliRunner.run should succeed (mock JSON parsed)");
    assert_eq!(
        payload.get("response").and_then(|v| v.as_str()),
        Some("done"),
        "mock gemini JSON did not round-trip through the real run path: {payload:?}"
    );

    // 2. Worktree write ALLOWED: default-confine didn't break legitimate work.
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

/// Build the mock gemini CLI script body. Absolute paths are baked in so the
/// child doesn't depend on cwd/TMPDIR.
fn mock_gemini_script(outside_create: &Path, outside_victim: &Path, inside_file: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # (a) ATTEMPT escape: create a file outside the worktree (jail must deny)\n\
         echo pwned > '{create}' 2>/dev/null || :\n\
         # (a) ATTEMPT escape: delete a pre-existing outside file (jail must deny)\n\
         rm -f '{victim}' 2>/dev/null || :\n\
         # (b) legit: write inside the worktree (jail must allow)\n\
         echo done > '{inside}'\n\
         # (c) emit gemini-CLI-shaped JSON on stdout for the runner to parse\n\
         printf '{{\"response\":\"done\",\"stats\":{{}}}}\\n'\n",
        create = outside_create.display(),
        victim = outside_victim.display(),
        inside = inside_file.display(),
    )
}

/// Create a fresh directory under the temp dir and return its CANONICAL path
/// (macOS `/tmp`→`/private/tmp`, so it matches the sandbox jail's real-path view).
/// Used for the worktree — which the jail DOES make writable.
fn fresh_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create dir");
    std::fs::canonicalize(&p).expect("canonicalize dir")
}

/// A fresh dir GUARANTEED outside the delegate's writable set, for the escape
/// target. The codex-model jail's writable set is: the worktree, real `$TMPDIR`,
/// `/tmp`, and the CLI's own state dir (none for gemini). `$HOME` at large is
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
