//! Runs a stand-in `claude` through [`RealSubprocessRunner::run`], under the real OS sandbox
//! (Seatbelt or bubblewrap), and checks that its JSON comes back, that it can write the worktree
//! and `~/.claude`, and that it cannot create or delete anything else.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tars_llm::provider::backends::claude_cli::{
    ClaudeToolAccess, RealSubprocessRunner, SubprocessInvocation, SubprocessRunner,
};

#[test]
fn delegate_cli_escape_blocked_through_real_run_path() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async { escape_blocked_async().await });
}

async fn escape_blocked_async() {
    let tag = format!("tars_delegate_{}", std::process::id());

    let worktree = fresh_dir(&format!("{tag}_wt"));
    let inside_file = worktree.join("inside.txt");

    let outside_dir = fresh_denied_dir(&format!("{tag}_outside"));
    let outside_create = outside_dir.join("escaped.txt");
    let outside_victim = outside_dir.join("victim.txt");
    std::fs::write(&outside_victim, b"i must survive").expect("seed outside victim");

    // claude's Bash tool makes `~/.claude/session-env/<id>` on first use. The jail grants only
    // state dirs that exist, so `~/.claude` is made first.
    let claude_home = PathBuf::from(std::env::var_os("HOME").expect("HOME set")).join(".claude");
    std::fs::create_dir_all(&claude_home).expect("ensure ~/.claude exists");
    let claude_probe = claude_home.join("session-env").join(&tag);

    let script = mock_cli_script(
        &outside_create,
        &outside_victim,
        &inside_file,
        &claude_probe,
    );
    // In the worktree: bubblewrap's mount namespace may hide other paths, and the worktree is
    // the one place both jails are sure to run it from.
    let script_path = worktree.join("mock_claude.sh");
    std::fs::write(&script_path, script).expect("write mock CLI");
    make_executable(&script_path);

    let inv = SubprocessInvocation {
        executable: script_path.to_string_lossy().into_owned(),
        model: "mock-model".into(),
        system: None,
        prompt: "please escape the worktree".into(),
        timeout: Duration::from_secs(30),
        stripped_env: HashSet::new(),
        tools: ClaudeToolAccess::default(),
        bare: false,
        effort: None,
        exclude_dynamic_sections: false,
        extra_args: Vec::new(),
        cwd: Some(worktree.clone()),
        // The default policy, `DangerFullAccess`, is jailed to the worktree all the same.
        sandbox: tars_sandbox::SandboxPolicy::default(),
    };

    let result = RealSubprocessRunner.run(inv).await;

    let payload = result
        .expect("RealSubprocessRunner.run should succeed (mock JSON parsed)")
        .payload;
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

    assert!(
        claude_probe.exists(),
        "claude state dir was NOT writable in the jail: `mkdir {}` was denied — \
         RealSubprocessRunner must grant claude's ~/.claude (session-env) writable",
        claude_probe.display()
    );

    let _ = std::fs::remove_dir_all(&worktree);
    let _ = std::fs::remove_dir_all(&outside_dir);
    let _ = std::fs::remove_dir_all(&claude_probe);
}

/// Returns a stand-in `claude` that reads its prompt, tries to create and delete outside the
/// worktree, writes inside it and under `~/.claude`, and prints a claude result. A denied write
/// does not stop it (`|| :`), so it still answers.
fn mock_cli_script(
    outside_create: &Path,
    outside_victim: &Path,
    inside_file: &Path,
    claude_probe: &Path,
) -> String {
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
         # (c2) legit: mkdir under ~/.claude/session-env — EXACTLY what claude's\n\
         #      Bash tool does on first use. The jail MUST grant claude's state\n\
         #      dir writable, else this EPERMs and the delegate can't run bash.\n\
         mkdir -p '{claude}' 2>/dev/null || :\n\
         # (d) emit claude-CLI-shaped JSON on stdout for the runner to parse\n\
         printf '{{\"type\":\"result\",\"result\":\"done\",\"is_error\":false}}\\n'\n",
        create = outside_create.display(),
        victim = outside_victim.display(),
        inside = inside_file.display(),
        claude = claude_probe.display(),
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
/// worktree, `$TMPDIR`, `/tmp` and claude's `~/.claude`.
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
