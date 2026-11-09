//! `fs.write_file`: creates or overwrites a file, creating its parent directories.
//!
//! Where it may write is the write jail's decision (see `write_jail`): under the tool's root
//! and the sandbox's writable roots, never under `ReadOnly` or a protected git directory, and
//! never through a symlink. A refused write is an `is_error` result the model can recover from,
//! and so is a path the filesystem rejects for its shape or its permissions (a component that is
//! a file, a target that is a directory, a file it may not open).

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use serde::Deserialize;

use tars_types::JsonSchema;

use super::write_jail::{WriteAccess, WriteError, WriteJail, replace_contents};
use crate::tool::{FsOp, Tool, ToolEnv, ToolError, ToolOutput};
use tars_sandbox::{ResolveRootError, canonical_root};

/// Caps the bytes one write may carry unless the tool is built with another cap.
pub(super) const DEFAULT_MAX_BYTES: NonZeroUsize =
    NonZeroUsize::new(1 << 20).expect("1 MiB is not zero");

pub struct WriteFileTool {
    /// Holds the canonical directory a resolved path must sit inside, when set.
    root: Option<PathBuf>,
    max_bytes: NonZeroUsize,
}

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

impl WriteFileTool {
    /// Returns a tool with no root of its own; it writes anywhere the process can unless
    /// [`ToolEnv::sandbox`] confines it.
    pub fn new() -> Self {
        Self {
            root: None,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Confines writes to `root`, refusing a path outside it with an `is_error` result.
    ///
    /// # Errors
    ///
    /// Fails when `root` cannot be canonicalized.
    pub fn with_root(root: impl AsRef<Path>) -> Result<Self, ResolveRootError> {
        Ok(Self {
            root: Some(canonical_root(root.as_ref())?),
            max_bytes: DEFAULT_MAX_BYTES,
        })
    }

    /// Sets the byte cap.
    pub fn max_bytes(mut self, n: NonZeroUsize) -> Self {
        self.max_bytes = n;
        self
    }
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "fs.write_file"
    }

    fn description(&self) -> &str {
        "Create a new file or overwrite an existing one with the given \
         contents. Use for whole-file writes; for a small in-place change \
         to a large file prefer fs.edit_file. Parent directories are \
         created as needed."
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "WriteFileArgs",
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path. Absolute or relative to the working directory."
                        },
                        "content": {
                            "type": "string",
                            "description": "Full new file contents (overwrites)."
                        }
                    },
                    "required": ["path", "content"]
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
        let parsed: WriteFileArgs =
            serde_json::from_value(args).map_err(ToolError::InvalidArguments)?;

        if parsed.content.len() > self.max_bytes.get() {
            return Ok(ToolOutput::error(format!(
                "refusing to write {} bytes; cap is {} (fs.write_file)",
                parsed.content.len(),
                self.max_bytes
            )));
        }

        let admitted = WriteJail::of(self.root.as_deref(), env)
            .and_then(|jail| jail.admit_create(Path::new(&parsed.path)));
        let target = match admitted {
            Ok(t) => t,
            Err(e) => return e.into_tool(),
        };
        let content = parsed.content;
        let written = target.path();

        // Cancellation returns at once; a write already handed to the blocking pool finishes.
        let write = tokio::task::spawn_blocking(move || {
            let mut file = target.open(WriteAccess::Create)?;
            replace_contents(&mut file, content.as_bytes()).map_err(|source| WriteError::Io {
                op: FsOp::Write,
                path: target.path(),
                source,
            })?;
            Ok::<_, WriteError>(content.len())
        });
        let joined = tokio::select! {
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            r = write => r,
        };
        match joined {
            Ok(Ok(len)) => Ok(ToolOutput::titled_success(
                format!(
                    "Wrote {}",
                    written.file_name().unwrap_or_default().to_string_lossy()
                ),
                format!("wrote {len} bytes to {}", written.display()),
            )),
            Ok(Err(e)) => e.into_tool(),
            Err(source) => Err(ToolError::TaskLost {
                tool: "fs.write_file",
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use crate::SandboxPolicy;

    fn ctx(cwd: Option<PathBuf>) -> ToolEnv {
        ToolEnv {
            cwd,
            ..Default::default()
        }
    }

    #[test]
    fn the_tool_is_named_fs_write_file() {
        assert_eq!(WriteFileTool::new().name(), "fs.write_file");
    }

    #[tokio::test]
    async fn writes_relative_to_cwd_and_creates_parents() {
        let dir = std::env::temp_dir().join(format!("tars_wf_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let tool = WriteFileTool::new();
        let r = tool
            .execute(
                json!({ "path": "sub/hello.txt", "content": "hi" }),
                &ctx(Some(dir.clone())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/hello.txt")).unwrap(),
            "hi"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn jail_rejects_escape() {
        let dir = std::env::temp_dir().join(format!("tars_wf_jail_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tool = WriteFileTool::with_root(&dir).unwrap();
        let r = tool
            .execute(
                json!({ "path": "../escape.txt", "content": "x" }),
                &ctx(Some(dir.clone())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "jail must reject ../ escape");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ctx_sandboxed(cwd: Option<PathBuf>, sandbox: SandboxPolicy) -> ToolEnv {
        ToolEnv {
            cwd,
            sandbox,
            ..Default::default()
        }
    }

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    async fn write_hook(cwd: PathBuf, root: PathBuf) -> ToolOutput {
        WriteFileTool::new()
            .execute(
                json!({"path": "../.git/hooks/pre-commit", "content": "#!/bin/sh\n"}),
                &ctx_sandboxed(Some(cwd), SandboxPolicy::workspace_write(&root)),
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_empty_git_in_the_workdir_does_not_open_the_repositorys_hooks() {
        let (_d, repo) = canonical_tempdir();
        std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(repo.join("pkg/.git")).unwrap();
        let r = write_hook(repo.join("pkg"), repo.clone()).await;
        assert!(r.is_error, "{}", r.content);
        assert!(!repo.join(".git/hooks/pre-commit").exists());
    }

    #[tokio::test]
    async fn a_submodule_workdir_does_not_open_the_superprojects_hooks() {
        let (_d, repo) = canonical_tempdir();
        std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(repo.join(".git/modules/sub")).unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        let r = write_hook(repo.join("sub"), repo.clone()).await;
        assert!(r.is_error, "{}", r.content);
        assert!(!repo.join(".git/hooks/pre-commit").exists());
    }

    #[tokio::test]
    async fn a_git_file_naming_a_pruned_worktree_still_lets_the_write_through() {
        let (_d, wt) = canonical_tempdir();
        std::fs::write(
            wt.join(".git"),
            "gitdir: /nonexistent/repo/.git/worktrees/wt\n",
        )
        .unwrap();
        let r = WriteFileTool::new()
            .execute(
                json!({"path": "a.txt", "content": "x"}),
                &ctx_sandboxed(Some(wt.clone()), SandboxPolicy::workspace_write(&wt)),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert!(wt.join("a.txt").exists());
    }

    #[tokio::test]
    async fn workspace_write_allows_inside_and_rejects_outside() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sandbox = SandboxPolicy::workspace_write(dir.path());

        let tool = WriteFileTool::new();
        let inside = tool
            .execute(
                json!({ "path": "sub/ok.txt", "content": "hi" }),
                &ctx_sandboxed(Some(dir.path().to_path_buf()), sandbox.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !inside.is_error,
            "write inside writable_root must succeed: {}",
            inside.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/ok.txt")).unwrap(),
            "hi"
        );

        let escapee = outside.path().join("escape.txt");
        let r = tool
            .execute(
                json!({ "path": escapee.to_str().unwrap(), "content": "x" }),
                &ctx_sandboxed(Some(dir.path().to_path_buf()), sandbox),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "write outside writable_root must be rejected");
        assert!(
            r.content.contains("outside the allowed write root"),
            "got: {}",
            r.content
        );
        assert!(!escapee.exists(), "rejected write must not create the file");
    }

    #[tokio::test]
    async fn read_only_rejects_all_writes() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new();
        let r = tool
            .execute(
                json!({ "path": "in_workspace.txt", "content": "x" }),
                &ctx_sandboxed(
                    Some(dir.path().to_path_buf()),
                    SandboxPolicy::read_only(false),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "read-only sandbox must reject writes");
        assert!(r.content.contains("read-only"), "got: {}", r.content);
        assert!(!dir.path().join("in_workspace.txt").exists());
    }

    #[tokio::test]
    async fn danger_full_access_imposes_no_new_restriction() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("anywhere.txt");
        let tool = WriteFileTool::new();
        let r = tool
            .execute(
                json!({ "path": target.to_str().unwrap(), "content": "ok" }),
                &ctx_sandboxed(None, SandboxPolicy::default()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !r.is_error,
            "default policy must not restrict: {}",
            r.content
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ok");
    }

    #[tokio::test]
    async fn toctou_symlinked_parent_swap_cannot_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        let sandbox = SandboxPolicy::workspace_write(root.path());
        let tool = WriteFileTool::new();
        // The symlink exists before the call, so the first check refuses it; the second check
        // is not reached.
        let r = tool
            .execute(
                json!({ "path": link.join("pwned.txt").to_str().unwrap(), "content": "x" }),
                &ctx_sandboxed(Some(root.path().to_path_buf()), sandbox),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            r.is_error,
            "write through a symlink pointing outside must be denied"
        );
        assert!(
            !outside.path().join("pwned.txt").exists(),
            "no bytes may land outside"
        );
    }

    #[tokio::test]
    async fn a_dangling_leaf_symlink_out_of_the_root_is_not_written_through() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let escapee = outside.path().join("not_yet.txt");
        let leaf = root.path().join("leaf.txt");
        std::os::unix::fs::symlink(&escapee, &leaf).unwrap();

        let r = WriteFileTool::new()
            .execute(
                json!({ "path": leaf.to_str().unwrap(), "content": "x" }),
                &ctx_sandboxed(
                    Some(root.path().to_path_buf()),
                    SandboxPolicy::workspace_write(root.path()),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            r.is_error,
            "a dangling leaf link must be refused: {}",
            r.content
        );
        assert!(!escapee.exists(), "no bytes may land outside");
    }

    #[tokio::test]
    async fn a_leaf_symlink_inside_the_root_is_not_followed_either() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real.txt");
        std::fs::write(&real, "before").unwrap();
        let leaf = root.path().join("leaf.txt");
        std::os::unix::fs::symlink(&real, &leaf).unwrap();

        let r = WriteFileTool::with_root(root.path())
            .unwrap()
            .execute(
                json!({ "path": "leaf.txt", "content": "after" }),
                &ctx(Some(root.path().to_path_buf())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("symbolic link"), "{}", r.content);
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "before");
    }

    #[tokio::test]
    async fn an_unconfined_write_follows_a_leaf_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        let leaf = dir.path().join("leaf.txt");
        std::os::unix::fs::symlink(&real, &leaf).unwrap();

        let r = WriteFileTool::new()
            .execute(
                json!({ "path": leaf.to_str().unwrap(), "content": "through" }),
                &ctx(None),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "through");
    }

    #[tokio::test]
    async fn a_writable_root_that_does_not_resolve_refuses_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let r = WriteFileTool::new()
            .execute(
                json!({ "path": "a.txt", "content": "x" }),
                &ctx_sandboxed(
                    Some(dir.path().to_path_buf()),
                    SandboxPolicy::workspace_write(&gone),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("cannot be resolved"), "{}", r.content);
        assert!(!dir.path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn a_refused_write_leaves_no_directories_outside() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let deep = outside.path().join("a/b/c.txt");
        let r = WriteFileTool::new()
            .execute(
                json!({ "path": deep.to_str().unwrap(), "content": "x" }),
                &ctx_sandboxed(
                    Some(root.path().to_path_buf()),
                    SandboxPolicy::workspace_write(root.path()),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(!outside.path().join("a").exists());
    }

    #[tokio::test]
    async fn dot_dot_after_a_missing_directory_creates_nothing_outside_the_root() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let r = WriteFileTool::with_root(&root)
            .unwrap()
            .execute(
                json!({ "path": "nope/../../escaped_dir/x.txt", "content": "x" }),
                &ctx(Some(root.clone())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert!(!base.path().join("escaped_dir").exists());
        assert!(!root.join("nope").exists());
    }

    #[tokio::test]
    async fn dot_dot_after_a_missing_directory_under_workspace_write_creates_nothing_outside() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let r = WriteFileTool::new()
            .execute(
                json!({ "path": "nope/../../escaped_ww/a/b/x.txt", "content": "x" }),
                &ctx_sandboxed(Some(root.clone()), SandboxPolicy::workspace_write(&root)),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert!(!base.path().join("escaped_ww").exists());
    }

    #[tokio::test]
    async fn dot_dot_through_an_existing_directory_still_writes_inside() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("a")).unwrap();
        let r = WriteFileTool::with_root(root.path())
            .unwrap()
            .execute(
                json!({ "path": "a/../b/x.txt", "content": "x" }),
                &ctx(Some(root.path().to_path_buf())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert!(root.path().join("b/x.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hard_link_to_a_file_outside_the_root_is_not_written() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = base.path().join("outside.txt");
        std::fs::write(&outside, "before").unwrap();
        std::fs::hard_link(&outside, root.join("leaf.txt")).unwrap();
        let r = WriteFileTool::with_root(&root)
            .unwrap()
            .execute(
                json!({ "path": "leaf.txt", "content": "after" }),
                &ctx_sandboxed(Some(root.clone()), SandboxPolicy::workspace_write(&root)),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "before");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_fifo_at_the_leaf_is_refused_without_blocking() {
        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let tool = WriteFileTool::with_root(root.path()).unwrap();
        let env = ctx(Some(root.path().to_path_buf()));
        let write = tool.execute(
            json!({ "path": "pipe", "content": "x" }),
            &env,
            CancellationToken::new(),
        );
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), write)
            .await
            .expect("the open does not block on a FIFO")
            .unwrap();
        assert!(r.is_error, "{}", r.content);
    }

    #[tokio::test]
    async fn a_component_that_is_a_file_is_the_models_to_fix() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "x").unwrap();
        let r = WriteFileTool::new()
            .execute(
                json!({ "path": "file/inner.txt", "content": "x" }),
                &ctx(Some(root.path().to_path_buf())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
    }
}
