//! Runs real commands under the write-jail and checks what they could write.
//!
//! The files outside the worktree live in `$HOME`, not `/tmp`: bubblewrap mounts a fresh tmpfs
//! over `/tmp`, so inside the jail a host `/tmp` file is invisible and reading or deleting it would
//! test the tmpfs, not the jail. `$HOME` is visible and read-only under `--ro-bind / /` on Linux
//! and write-denied by Seatbelt on macOS. Creating a file in `/tmp` is still checked: the host
//! `/tmp` must not get it on either OS.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use tars_sandbox::SandboxPolicy;

#[cfg(target_os = "macos")]
const WRAPPER: &str = "/usr/bin/sandbox-exec";
#[cfg(target_os = "linux")]
const WRAPPER: &str = "bwrap";

fn wrapper_present() -> bool {
    Command::new(WRAPPER).arg("--version").output().is_ok()
}

fn uniq(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tars_sbx_{tag}_{}", std::process::id()))
}

fn run_jailed(worktree: &Path, script: &str) -> bool {
    let wrapped = SandboxPolicy::workspace_write(worktree)
        .wrap("/bin/sh", &["-c".into(), script.into()], worktree)
        .expect("wrap builds");
    Command::new(wrapped.program)
        .args(wrapped.args)
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

    let wt = uniq("wt");
    let _ = std::fs::remove_dir_all(&wt);
    std::fs::create_dir_all(&wt).unwrap();
    let wt = std::fs::canonicalize(&wt).unwrap();

    let out_tmp = uniq("outside_tmp");
    let _ = std::fs::remove_file(&out_tmp);

    let home = std::env::var("HOME").unwrap();
    let out_home = format!("{home}/tars_sbx_pwn_{}", std::process::id());
    let _ = std::fs::remove_file(&out_home);
    let victim = format!("{home}/tars_sbx_victim_{}", std::process::id());
    std::fs::write(&victim, b"i must survive").unwrap();

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

    let _ = run_jailed(&wt, &format!("echo pwned > {}", out_tmp.display()));
    assert!(!out_tmp.exists(), "must NOT create a file in the host /tmp");

    let _ = run_jailed(&wt, &format!("echo pwned > {out_home}"));
    assert!(
        !Path::new(&out_home).exists(),
        "must NOT create a file in $HOME"
    );

    let _ = run_jailed(&wt, &format!("rm -f {victim}"));
    assert!(
        Path::new(&victim).exists(),
        "must NOT delete a file outside the worktree"
    );
    assert_eq!(std::fs::read(&victim).unwrap(), b"i must survive");

    assert!(
        run_jailed(&wt, &format!("cat {victim} > /dev/null")),
        "reading outside is allowed under the write-jail (codex model)"
    );

    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_file(&victim);
    let _ = std::fs::remove_file(&out_tmp);
    let _ = std::fs::remove_file(&out_home);
}

#[test]
fn a_read_only_jail_does_not_write_in_its_workdir() {
    if !wrapper_present() {
        eprintln!("skipping: sandbox wrapper `{WRAPPER}` not present on this host");
        return;
    }
    let wt = uniq("ro_wt");
    let _ = std::fs::remove_dir_all(&wt);
    std::fs::create_dir_all(&wt).unwrap();
    let wt = std::fs::canonicalize(&wt).unwrap();
    let policy = SandboxPolicy::read_only(false);
    let target = wt.join("written.txt");
    let wrapped = policy
        .wrap(
            "/bin/sh",
            &["-c".into(), format!("echo x > {}", target.display())],
            &wt,
        )
        .expect("wrap builds");
    let _ = Command::new(wrapped.program)
        .args(wrapped.args)
        .status()
        .expect("spawn");
    assert!(!target.exists(), "read-only must not write in its workdir");
    let _ = std::fs::remove_dir_all(&wt);
}

#[test]
fn the_repositorys_git_is_not_writable_from_a_subdirectory() {
    if !wrapper_present() {
        eprintln!("skipping: sandbox wrapper `{WRAPPER}` not present on this host");
        return;
    }
    let repo = uniq("repo");
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(repo.join("pkg")).unwrap();
    let repo = std::fs::canonicalize(&repo).unwrap();
    let hook = repo.join(".git/hooks/pre-commit");
    let policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![repo.clone()],
        network: false,
    };
    let wrapped = policy
        .wrap(
            "/bin/sh",
            &["-c".into(), format!("echo pwned > {}", hook.display())],
            &repo.join("pkg"),
        )
        .expect("wrap builds");
    let _ = Command::new(wrapped.program)
        .args(wrapped.args)
        .current_dir(repo.join("pkg"))
        .status()
        .expect("spawn");
    assert!(!hook.exists(), "the repository's hooks were written");
    let _ = std::fs::remove_dir_all(&repo);
}
