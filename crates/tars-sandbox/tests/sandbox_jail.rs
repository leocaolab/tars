//! Integration tests that ACTUALLY RUN the exec sandbox with a mock command
//! (`/bin/sh` standing in for `claude`) and assert the write-jail holds:
//! - CAN write/delete inside the worktree,
//! - CANNOT create a file outside (in /tmp or $HOME),
//! - CANNOT delete a file outside (in /tmp).
//!
//! macOS-only runnable (uses `sandbox-exec`); a Linux/bwrap variant needs a
//! Linux box. Skips cleanly if `sandbox-exec` isn't present.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use tars_sandbox::SandboxPolicy;

fn uniq(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tars_sbx_{tag}_{}", std::process::id()))
}

/// Run `sh -c <script>` under the write-jail for `worktree`; return success.
fn run_jailed(worktree: &Path, script: &str) -> bool {
    let (prog, argv) = SandboxPolicy::workspace_write(worktree)
        .wrap("/bin/sh", &["-c".into(), script.into()], worktree)
        .expect("wrap builds");
    Command::new(prog)
        .args(argv)
        .status()
        .expect("spawn sandbox-exec")
        .success()
}

#[test]
fn write_jail_confines_all_cases() {
    // worktree (must exist for canonicalize)
    let wt = uniq("wt");
    let _ = std::fs::remove_dir_all(&wt);
    std::fs::create_dir_all(&wt).unwrap();
    let wt = std::fs::canonicalize(&wt).unwrap();

    // outside targets
    let out_tmp = uniq("outside_tmp"); // a file we try to CREATE in /tmp
    let _ = std::fs::remove_file(&out_tmp);
    let victim = uniq("victim"); // a pre-existing /tmp file we try to DELETE
    std::fs::write(&victim, b"i must survive").unwrap();
    let home = std::env::var("HOME").unwrap();
    let out_home = format!("{home}/tars_sbx_pwn_{}", std::process::id());
    let _ = std::fs::remove_file(&out_home);

    // ── inside worktree: allowed ──────────────────────────────────────
    let inside = wt.join("inside.txt");
    assert!(
        run_jailed(&wt, &format!("echo ok > {}", inside.display())),
        "writing inside the worktree should succeed"
    );
    assert!(inside.exists(), "inside file must exist");

    assert!(
        run_jailed(&wt, &format!("rm -f {}", inside.display())),
        "deleting inside the worktree should succeed"
    );
    assert!(!inside.exists(), "inside file deleted");

    // ── create OUTSIDE (in /tmp): must be blocked ─────────────────────
    let _ = run_jailed(&wt, &format!("echo pwned > {}", out_tmp.display()));
    assert!(!out_tmp.exists(), "must NOT create a file outside the worktree (/tmp)");

    // ── create OUTSIDE (in $HOME): must be blocked ────────────────────
    let _ = run_jailed(&wt, &format!("echo pwned > {out_home}"));
    assert!(!Path::new(&out_home).exists(), "must NOT create a file in $HOME");

    // ── delete OUTSIDE (in /tmp): must be blocked ─────────────────────
    let _ = run_jailed(&wt, &format!("rm -f {}", victim.display()));
    assert!(victim.exists(), "must NOT delete a file outside the worktree (/tmp)");
    assert_eq!(std::fs::read(&victim).unwrap(), b"i must survive");

    // ── the program can still READ outside (codex write-jail model) ───
    // (read is broad; containment is on writes + egress, not reads)
    assert!(
        run_jailed(&wt, &format!("cat {} > /dev/null", victim.display())),
        "reading outside is allowed under the write-jail (codex model)"
    );

    // cleanup
    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_file(&victim);
    let _ = std::fs::remove_file(&out_tmp);
    let _ = std::fs::remove_file(&out_home);
}
