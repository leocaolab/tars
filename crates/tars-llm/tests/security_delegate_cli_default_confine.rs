//! Checks that a non-claude CLI given the default policy (`DangerFullAccess`) is still jailed to
//! its worktree by the real OS sandbox (Seatbelt or bubblewrap), so it runs only on macOS and
//! Linux.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tars_llm::provider::backends::cli::{
    AntigravityDialect, AntigravityEffort, SharedCliRunner, SubprocessInvocation, SubprocessRunner,
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
    let tag = format!("tars_agy_default_{}", std::process::id());

    let worktree = fresh_dir(&format!("{tag}_wt"));
    let inside_file = worktree.join("inside.txt");

    // Under `$HOME`, not `$TMPDIR`: the jail lets the child write `$TMPDIR` and `/tmp`.
    let outside_dir = fresh_denied_dir(&format!("{tag}_outside"));
    let outside_create = outside_dir.join("escaped.txt");
    let outside_victim = outside_dir.join("victim.txt");
    std::fs::write(&outside_victim, b"i must survive").expect("seed outside victim");

    let script = mock_agy_script(&outside_create, &outside_victim, &inside_file);
    let script_path = outside_dir.join("mock_agy.sh");
    std::fs::write(&script_path, script).expect("write mock CLI");
    make_executable(&script_path);

    let inv = SubprocessInvocation::neutral(
        script_path.to_string_lossy().into_owned(),
        "mock-agy-model".into(),
        "please escape the worktree".into(),
        Duration::from_secs(30),
        HashSet::new(),
        Some(worktree.clone()),
        tars_sandbox::SandboxPolicy::default(),
    );

    let dialect = Arc::new(AntigravityDialect::new(
        "agy".into(),
        Duration::from_secs(30),
        AntigravityEffort::High,
    ));
    let runner = SharedCliRunner::new(dialect);
    let result = runner.run(inv).await;

    // A text dialect's runner returns stdout as a JSON string.
    let payload = result
        .expect("SharedCliRunner.run should succeed (mock stdout drained)")
        .payload;
    assert_eq!(
        payload.as_str().map(str::trim),
        Some("done"),
        "mock agy stdout did not round-trip through the real run path: {payload:?}"
    );

    assert!(
        inside_file.exists(),
        "delegate should be able to write inside the worktree ({})",
        inside_file.display()
    );

    assert!(
        !outside_create.exists(),
        "SANDBOX ESCAPE: delegate created a file OUTSIDE the worktree ({})",
        outside_create.display()
    );

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

    let _ = std::fs::remove_dir_all(&worktree);
    let _ = std::fs::remove_dir_all(&outside_dir);
}

/// Returns a stand-in `agy` that tries to create and delete outside the worktree, writes inside
/// it and prints `done`. A denied write does not stop it (`|| :`), so it still answers.
fn mock_agy_script(outside_create: &Path, outside_victim: &Path, inside_file: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # (a) ATTEMPT escape: create a file outside the worktree (jail must deny)\n\
         echo pwned > '{create}' 2>/dev/null || :\n\
         # (a) ATTEMPT escape: delete a pre-existing outside file (jail must deny)\n\
         rm -f '{victim}' 2>/dev/null || :\n\
         # (b) legit: write inside the worktree (jail must allow)\n\
         echo done > '{inside}'\n\
         # (c) emit a plain-text answer on stdout (agy is OutputMode::Text)\n\
         printf 'done\\n'\n",
        create = outside_create.display(),
        victim = outside_victim.display(),
        inside = inside_file.display(),
    )
}

/// Creates an empty directory under the temp dir and returns its canonical path, which is what
/// the jail sees (on macOS `/tmp` is `/private/tmp`).
fn fresh_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create dir");
    std::fs::canonicalize(&p).expect("canonicalize dir")
}

/// Creates an empty directory under `$HOME`, outside what the jail lets the child write: the
/// worktree, `$TMPDIR`, `/tmp` and the CLI's state dirs (none for agy).
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
