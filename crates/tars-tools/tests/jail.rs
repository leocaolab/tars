//! Drives the filesystem tools against the jails on a real disk: a directory swapped for a
//! symlink mid-call, FIFOs, a stale writable root, a file it may not open, the workdir's `.git`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;
use tars_tools::builtins::{EditFileTool, ReadFileTool, WriteFileTool};
use tars_tools::{SandboxPolicy, Tool, ToolEnv};
use tokio_util::sync::CancellationToken;

fn ctx(cwd: &Path, sandbox: SandboxPolicy) -> ToolEnv {
    ToolEnv {
        cwd: Some(cwd.to_path_buf()),
        sandbox,
        ..Default::default()
    }
}

fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path()).unwrap();
    (dir, path)
}

fn mkfifo(p: &Path) {
    let made = std::process::Command::new("mkfifo")
        .arg(p)
        .status()
        .unwrap();
    assert!(made.success());
}

/// Flips `link` between the real directory `real` and a symlink to `outside` until `stop`.
fn flipper(link: PathBuf, real: PathBuf, outside: PathBuf, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let _ = std::fs::rename(&real, &link);
            std::thread::yield_now();
            let _ = std::fs::rename(&link, &real);
            let _ = std::os::unix::fs::symlink(&outside, &link);
            std::thread::yield_now();
            let _ = std::fs::remove_file(&link);
        }
    });
}

const RACE_ROUNDS: usize = 5000;

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_swapped_for_a_symlink_does_not_lead_a_write_out_of_the_root() {
    let (_d, base) = canonical_tempdir();
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(root.join("real")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    flipper(
        root.join("d"),
        root.join("real"),
        outside.clone(),
        stop.clone(),
    );
    let tool = WriteFileTool::with_root(&root).unwrap();
    for _ in 0..RACE_ROUNDS {
        let _ = tool
            .execute(
                json!({"path": "d/x.txt", "content": "escaped"}),
                &ctx(&root, SandboxPolicy::workspace_write(&root)),
                CancellationToken::new(),
            )
            .await;
        if outside.join("x.txt").exists() {
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    assert!(
        !outside.join("x.txt").exists(),
        "a confined write landed outside the root"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_swapped_for_a_symlink_does_not_lead_a_read_out_of_the_root() {
    let (_d, base) = canonical_tempdir();
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(root.join("real")).unwrap();
    std::fs::write(root.join("real/s.txt"), "inside").unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("s.txt"), "SECRET").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    flipper(
        root.join("d"),
        root.join("real"),
        outside.clone(),
        stop.clone(),
    );
    let tool = ReadFileTool::with_root(&root).unwrap();
    let mut leaked = false;
    for _ in 0..RACE_ROUNDS {
        if let Ok(r) = tool
            .execute(
                json!({"path": "d/s.txt"}),
                &ctx(&root, SandboxPolicy::workspace_write(&root)),
                CancellationToken::new(),
            )
            .await
            && r.content.contains("SECRET")
        {
            leaked = true;
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    assert!(!leaked, "a confined read returned a file outside the root");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_on_a_fifo_returns() {
    let (_d, root) = canonical_tempdir();
    mkfifo(&root.join("pipe"));
    for sandbox in [
        SandboxPolicy::workspace_write(&root),
        SandboxPolicy::default(),
    ] {
        let tool = EditFileTool::new();
        let env = ctx(&root, sandbox);
        let call = tool.execute(
            json!({"path": "pipe", "old_string": "a", "new_string": "b"}),
            &env,
            CancellationToken::new(),
        );
        let r = tokio::time::timeout(Duration::from_secs(3), call)
            .await
            .expect("fs.edit_file on a FIFO returns")
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("not a regular file"), "{}", r.content);
    }
}

#[tokio::test]
async fn an_edit_of_a_file_over_the_cap_is_refused() {
    let (_d, root) = canonical_tempdir();
    let big = "a".repeat((1 << 20) + 1);
    std::fs::write(root.join("big.txt"), &big).unwrap();
    let r = EditFileTool::new()
        .execute(
            json!({"path": "big.txt", "old_string": "a", "new_string": "b", "replace_all": true}),
            &ctx(&root, SandboxPolicy::workspace_write(&root)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(r.is_error, "{}", r.content);
    assert!(r.content.contains("byte cap"), "{}", r.content);
    assert_eq!(std::fs::read_to_string(root.join("big.txt")).unwrap(), big);
}

#[tokio::test]
async fn a_file_written_under_a_writable_root_outside_cwd_can_be_read_back() {
    let (_w, wt) = canonical_tempdir();
    let (_s, scratch) = canonical_tempdir();
    let sandbox = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![wt.clone(), scratch.clone()],
        network: true,
    };
    let target = scratch.join("note.txt");
    let w = WriteFileTool::new()
        .execute(
            json!({"path": target.to_str().unwrap(), "content": "hi"}),
            &ctx(&wt, sandbox.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!w.is_error, "{}", w.content);
    let r = ReadFileTool::new()
        .execute(
            json!({"path": target.to_str().unwrap()}),
            &ctx(&wt, sandbox),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!r.is_error, "{}", r.content);
}

#[tokio::test]
async fn a_writable_root_not_made_yet_is_dropped_and_the_rest_still_admit() {
    let (_w, wt) = canonical_tempdir();
    let stale = wt.join("scratch-not-made-yet");
    let sandbox = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![wt.clone(), stale.clone()],
        network: true,
    };
    let w = WriteFileTool::new()
        .execute(
            json!({"path": "a.txt", "content": "hi"}),
            &ctx(&wt, sandbox.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!w.is_error, "{}", w.content);

    let outside = tempfile::tempdir().unwrap();
    let refused = WriteFileTool::new()
        .execute(
            json!({"path": outside.path().join("b.txt").to_str().unwrap(), "content": "x"}),
            &ctx(&wt, sandbox),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(refused.is_error);
    assert!(
        refused.content.contains(&stale.display().to_string()),
        "the refusal names the dropped root: {}",
        refused.content
    );
}

#[tokio::test]
async fn a_file_it_may_not_open_is_the_models_to_route_around() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, root) = canonical_tempdir();
    let f = root.join("locked.txt");
    std::fs::write(&f, "x").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
    let c = || ctx(&root, SandboxPolicy::workspace_write(&root));
    let w = WriteFileTool::new()
        .execute(
            json!({"path": "locked.txt", "content": "y"}),
            &c(),
            CancellationToken::new(),
        )
        .await;
    let r = ReadFileTool::new()
        .execute(
            json!({"path": "locked.txt"}),
            &c(),
            CancellationToken::new(),
        )
        .await;
    let e = EditFileTool::new()
        .execute(
            json!({"path": "locked.txt", "old_string": "x", "new_string": "y"}),
            &c(),
            CancellationToken::new(),
        )
        .await;
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    for (tool, result) in [("write", w), ("read", r), ("edit", e)] {
        let result = result.unwrap_or_else(|err| panic!("{tool} ended as a ToolError: {err}"));
        assert!(result.is_error, "{tool}: {}", result.content);
    }
}

fn workspace_with_git() -> (tempfile::TempDir, PathBuf) {
    let (d, wt) = canonical_tempdir();
    std::fs::create_dir_all(wt.join(".git/hooks")).unwrap();
    std::fs::write(wt.join(".git/config"), "[core]\n").unwrap();
    (d, wt)
}

#[tokio::test]
async fn a_workspace_write_sandbox_refuses_a_write_under_the_workdirs_git() {
    let (_d, wt) = workspace_with_git();
    for path in [".git/hooks/pre-commit", ".git/new/dir/x"] {
        let r = WriteFileTool::new()
            .execute(
                json!({"path": path, "content": "#!/bin/sh\necho pwned\n"}),
                &ctx(&wt, SandboxPolicy::workspace_write(&wt)),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{path}: {}", r.content);
        assert!(!wt.join(path).exists(), "{path} was written");
    }
    let r = EditFileTool::new()
        .execute(
            json!({"path": ".git/config", "old_string": "[core]", "new_string": "[core]\n\thooksPath = /tmp"}), &ctx(&wt, SandboxPolicy::workspace_write(&wt)), CancellationToken::new())
        .await
        .unwrap();
    assert!(r.is_error, "{}", r.content);
    assert_eq!(
        std::fs::read_to_string(wt.join(".git/config")).unwrap(),
        "[core]\n"
    );
}

#[tokio::test]
async fn a_tool_root_without_a_sandbox_still_writes_under_git() {
    let (_d, wt) = workspace_with_git();
    let r = WriteFileTool::with_root(&wt)
        .unwrap()
        .execute(
            json!({"path": ".git/info", "content": "x"}),
            &ctx(&wt, SandboxPolicy::default()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!r.is_error, "{}", r.content);
}

#[tokio::test]
async fn an_unconfined_write_reaches_a_device() {
    let r = WriteFileTool::new()
        .execute(
            json!({"path": "/dev/null", "content": "x"}),
            &ToolEnv::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!r.is_error, "{}", r.content);
}

fn workspace_write_on(roots: Vec<PathBuf>) -> SandboxPolicy {
    SandboxPolicy::WorkspaceWrite {
        writable_roots: roots,
        network: true,
    }
}

#[tokio::test]
async fn a_rooted_tool_without_a_cwd_protects_the_git_of_its_root() {
    let (_d, wt) = workspace_with_git();
    let no_cwd = ToolEnv {
        cwd: None,
        sandbox: SandboxPolicy::workspace_write(&wt),
        ..Default::default()
    };
    let w = WriteFileTool::with_root(&wt)
        .unwrap()
        .execute(
            json!({"path": ".git/hooks/pre-commit", "content": "#!/bin/sh\necho pwned\n"}),
            &no_cwd.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(w.is_error, "{}", w.content);
    assert!(!wt.join(".git/hooks/pre-commit").exists());
    let e = EditFileTool::with_root(&wt)
        .unwrap()
        .execute(
            json!({"path": ".git/config", "old_string": "[core]", "new_string": "[core]\n\tfsmonitor = touch /tmp/pwned"}), &no_cwd, CancellationToken::new())
        .await
        .unwrap();
    assert!(e.is_error, "{}", e.content);
    assert_eq!(
        std::fs::read_to_string(wt.join(".git/config")).unwrap(),
        "[core]\n"
    );
}

#[tokio::test]
async fn a_symlinked_git_is_protected_where_it_points() {
    let (_d, wt) = canonical_tempdir();
    std::fs::create_dir_all(wt.join("gitdir/hooks")).unwrap();
    std::os::unix::fs::symlink(wt.join("gitdir"), wt.join(".git")).unwrap();
    for path in [".git/hooks/pre-commit", "gitdir/hooks/pre-commit"] {
        let r = WriteFileTool::new()
            .execute(
                json!({"path": path, "content": "#!/bin/sh\necho pwned\n"}),
                &ctx(&wt, SandboxPolicy::workspace_write(&wt)),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{path}: {}", r.content);
    }
    assert!(!wt.join("gitdir/hooks/pre-commit").exists());
}

#[tokio::test]
async fn a_workdir_below_the_repository_does_not_write_the_repositorys_git() {
    let (_d, repo) = workspace_with_git();
    std::fs::create_dir_all(repo.join("pkg")).unwrap();
    let r = WriteFileTool::new()
        .execute(
            json!({"path": "../.git/hooks/pre-commit", "content": "#!/bin/sh\necho pwned\n"}),
            &ctx(&repo.join("pkg"), workspace_write_on(vec![repo.clone()])),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(r.is_error, "{}", r.content);
    assert!(!repo.join(".git/hooks/pre-commit").exists());
    let ok = WriteFileTool::new()
        .execute(
            json!({"path": "src/a.rs", "content": "fn a() {}\n"}),
            &ctx(&repo.join("pkg"), workspace_write_on(vec![repo.clone()])),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!ok.is_error, "{}", ok.content);
}

#[tokio::test]
async fn a_socket_is_not_written_and_the_call_goes_on() {
    let (_d, root) = canonical_tempdir();
    let _l = std::os::unix::net::UnixListener::bind(root.join("s.sock")).unwrap();
    let w = WriteFileTool::new()
        .execute(
            json!({"path": "s.sock", "content": "x"}),
            &ctx(&root, SandboxPolicy::workspace_write(&root)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        w.is_error && w.content.contains("not a regular file"),
        "{}",
        w.content
    );
    let e = EditFileTool::new()
        .execute(
            json!({"path": "s.sock", "old_string": "a", "new_string": "b"}),
            &ctx(&root, SandboxPolicy::workspace_write(&root)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        e.is_error && e.content.contains("not a regular file"),
        "{}",
        e.content
    );
}

#[tokio::test]
async fn a_workdir_that_cannot_be_resolved_is_the_hosts_failure() {
    let (_d, base) = canonical_tempdir();
    let gone = base.join("not-made");
    let err = WriteFileTool::new()
        .execute(
            json!({"path": "a.txt", "content": "x"}),
            &ctx(&gone, SandboxPolicy::workspace_write(&base)),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, tars_tools::ToolError::WriteWorkdir(_)),
        "{err}"
    );
}
