//! Integration tests that ACTUALLY RUN the exec sandbox with a mock command
//! (`/bin/sh` standing in for `claude`) and assert the write-jail holds:
//! - CAN write/delete inside the worktree,
//! - CANNOT create a file outside (in /tmp or $HOME),
//! - CANNOT delete a file outside,
//! - CAN still read outside (codex write-jail model: containment is on writes).
//!
//! Runs on macOS (Seatbelt via `sandbox-exec`) and Linux (bubblewrap). Skips
//! cleanly if the platform sandbox wrapper binary isn't present.
//!
//! ## Platform note: why the "outside" victim lives in $HOME, not /tmp
//! macOS Seatbelt is a deny-write filter over a fully-visible fs, so any real
//! path works as an "outside" target. Linux bubblewrap is a mount namespace
//! whose `--tmpfs /tmp` masks the host `/tmp` with a fresh empty tmpfs — a file
//! seeded in the real `/tmp` is INVISIBLE inside the jail, so a `cat`/`rm` of it
//! neither reads nor deletes the host file (the "read outside is allowed" and
//! "delete outside is blocked" cases would then be testing tmpfs masking, not
//! the write-jail). `$HOME` is under the `--ro-bind / /` mount: visible AND
//! read-only inside the jail, so it exercises the real write-deny on BOTH OSes.
//! The create-in-`/tmp` case is kept (it still proves the host `/tmp` is never
//! written: macOS denies the write, Linux swallows it into the ephemeral tmpfs).

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use tars_sandbox::SandboxPolicy;

/// The OS sandbox wrapper binary spawned by `SandboxPolicy::wrap` on this
/// platform. The test skips if it isn't installed (e.g. CI without bwrap).
#[cfg(target_os = "macos")]
const WRAPPER: &str = "/usr/bin/sandbox-exec";
#[cfg(target_os = "linux")]
const WRAPPER: &str = "bwrap";

/// True if the platform wrapper binary can be launched. Dep-free (the crate is
/// deliberately zero-dependency), so we probe by spawning `--version`.
fn wrapper_present() -> bool {
    Command::new(WRAPPER).arg("--version").output().is_ok()
}

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
        .expect("spawn sandbox wrapper")
        .success()
}

#[test]
fn write_jail_confines_all_cases() {
    if !wrapper_present() {
        eprintln!("skipping: sandbox wrapper `{WRAPPER}` not present on this host");
        return;
    }

    // worktree (must exist for canonicalize) — the one writable root.
    let wt = uniq("wt");
    let _ = std::fs::remove_dir_all(&wt);
    std::fs::create_dir_all(&wt).unwrap();
    let wt = std::fs::canonicalize(&wt).unwrap();

    // ── outside targets ───────────────────────────────────────────────
    // create-in-/tmp: proves the host /tmp is never written (see module note).
    let out_tmp = uniq("outside_tmp");
    let _ = std::fs::remove_file(&out_tmp);

    // $HOME-based targets: visible-but-read-only inside BOTH jails, so the
    // delete/read cases exercise the real write-jail (not Linux tmpfs masking).
    let home = std::env::var("HOME").unwrap();
    let out_home = format!("{home}/tars_sbx_pwn_{}", std::process::id());
    let _ = std::fs::remove_file(&out_home);
    let victim = format!("{home}/tars_sbx_victim_{}", std::process::id());
    std::fs::write(&victim, b"i must survive").unwrap();

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

    // ── create OUTSIDE (in /tmp): host /tmp must never be written ──────
    let _ = run_jailed(&wt, &format!("echo pwned > {}", out_tmp.display()));
    assert!(!out_tmp.exists(), "must NOT create a file in the host /tmp");

    // ── create OUTSIDE (in $HOME): must be blocked by the write-jail ───
    let _ = run_jailed(&wt, &format!("echo pwned > {out_home}"));
    assert!(!Path::new(&out_home).exists(), "must NOT create a file in $HOME");

    // ── delete OUTSIDE (in $HOME): must be blocked ────────────────────
    let _ = run_jailed(&wt, &format!("rm -f {victim}"));
    assert!(Path::new(&victim).exists(), "must NOT delete a file outside the worktree");
    assert_eq!(std::fs::read(&victim).unwrap(), b"i must survive");

    // ── the program can still READ outside (codex write-jail model) ───
    // (read is broad; containment is on writes + egress, not reads)
    assert!(
        run_jailed(&wt, &format!("cat {victim} > /dev/null")),
        "reading outside is allowed under the write-jail (codex model)"
    );

    // cleanup
    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_file(&victim);
    let _ = std::fs::remove_file(&out_tmp);
    let _ = std::fs::remove_file(&out_home);
}
