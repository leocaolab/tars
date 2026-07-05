//! `fs.write_file` — create or overwrite a file with given contents.
//!
//! The write half of the filesystem builtins (sibling of
//! [`super::read_file`]). A tars-native coding Agent (Session loop over a
//! pure-inference provider) needs this to act on a worktree — the
//! `ToolContext::cwd` the Session sets IS the agent's working tree, so the
//! side effect lands exactly where the orchestrator scoped it, not in the
//! process cwd.
//!
//! Side effect: **Reversible** (Doc 04 §4.4) — a write can be rolled back
//! by restoring the prior bytes; the Session/worktree owns that snapshot.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use tars_types::JsonSchema;

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};
use crate::{SandboxMode, SandboxPolicy};

/// 1 MiB default cap on a single write — a coding edit, not a data dump.
const DEFAULT_MAX_BYTES: usize = 1 << 20;

pub struct WriteFileTool {
    /// If set, the resolved path must sit inside this directory. `None`
    /// = no jail (trusted CLI context only).
    root: Option<PathBuf>,
    /// Hard cap on bytes written per invocation.
    max_bytes: usize,
}

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    /// File path. Absolute or relative to [`ToolContext::cwd`].
    path: String,
    /// Full new contents. Overwrites any existing file.
    content: String,
}

impl WriteFileTool {
    /// Construct without a jail. Writes anywhere the process can. Tests +
    /// trusted local CLI use this.
    pub fn new() -> Self {
        Self {
            root: None,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Constrain writes to `root` (canonicalized eagerly). A path that
    /// resolves outside is rejected as a logical error (the model adapts).
    pub fn with_root(root: impl AsRef<Path>) -> Option<Self> {
        let canonical_root = std::fs::canonicalize(root.as_ref()).ok()?;
        Some(Self {
            root: Some(canonical_root),
            max_bytes: DEFAULT_MAX_BYTES,
        })
    }

    /// Override the max-bytes cap. Chainable.
    pub fn max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = n;
        self
    }

    /// The set of write constraints in force for this call. Each entry is a
    /// list of roots the target must sit under; the target must satisfy EVERY
    /// constraint (defense in depth = AND). Two independent sources:
    ///
    /// * `self.root` — the per-tool `with_root` jail (kept working).
    /// * `ctx.sandbox` — uniform confinement: `WorkspaceWrite` constrains to
    ///   the canonicalized `writable_roots` (a `WorkspaceWrite` with none →
    ///   an empty constraint that nothing satisfies → fail-closed deny-all).
    ///   `ReadOnly` is handled earlier (all writes denied). `DangerFullAccess`
    ///   adds no constraint, so the default policy imposes nothing new.
    ///
    /// A writable root that can't be canonicalized is dropped (fail-closed:
    /// fewer allowed roots, never more).
    fn write_constraints(&self, sandbox: &SandboxPolicy) -> Vec<Vec<PathBuf>> {
        let mut constraints: Vec<Vec<PathBuf>> = Vec::new();
        if let Some(root) = &self.root {
            constraints.push(vec![root.clone()]);
        }
        if sandbox.mode == SandboxMode::WorkspaceWrite {
            let roots: Vec<PathBuf> = sandbox
                .writable_roots
                .iter()
                .filter_map(|p| std::fs::canonicalize(p).ok())
                .collect();
            constraints.push(roots);
        }
        constraints
    }

    /// Resolve an input path against `cwd`, deny outright under `ReadOnly`,
    /// then (if any jail is in force) verify the nearest EXISTING ancestor —
    /// which defeats `..` traversal and existing-symlink escapes even when the
    /// leaf file doesn't exist yet. This is a cheap PRE-check; the actual write
    /// re-verifies the materialized parent's canonical path (TOCTOU close).
    fn resolve(
        &self,
        input: &str,
        cwd: Option<&Path>,
        sandbox: &SandboxPolicy,
    ) -> Result<PathBuf, ToolResult> {
        let raw = Path::new(input);
        let combined = if raw.is_absolute() {
            raw.to_path_buf()
        } else if let Some(cwd) = cwd {
            cwd.join(raw)
        } else {
            raw.to_path_buf()
        };

        // ReadOnly mode: no write may land anywhere. Report the real target.
        if sandbox.mode == SandboxMode::ReadOnly {
            return Err(ToolResult::error(format!(
                "write to `{}` denied: sandbox mode is read-only",
                combined.display()
            )));
        }

        let constraints = self.write_constraints(sandbox);
        if constraints.is_empty() {
            // No jail (DangerFullAccess + no `with_root`) — today's default,
            // unchanged: the un-canonicalized combined path is written as-is.
            return Ok(combined);
        }

        // Walk up to the nearest existing ancestor and canonicalize it.
        let mut anchor = combined.as_path();
        let existing = loop {
            if anchor.exists() {
                break anchor.to_path_buf();
            }
            match anchor.parent() {
                Some(p) => anchor = p,
                None => {
                    return Err(ToolResult::error(format!(
                        "cannot resolve a real ancestor of `{}`",
                        combined.display()
                    )));
                }
            }
        };
        let canonical_existing = std::fs::canonicalize(&existing).map_err(|e| {
            ToolResult::error(format!("cannot resolve `{}`: {e}", existing.display()))
        })?;
        if !satisfies(&canonical_existing, &constraints) {
            return Err(denied_outside(&combined, &constraints));
        }
        Ok(combined)
    }
}

/// True iff `canonical` sits under at least one root of EVERY constraint.
/// An empty constraint (e.g. `WorkspaceWrite` with no writable roots) is
/// satisfied by nothing — fail-closed.
fn satisfies(canonical: &Path, constraints: &[Vec<PathBuf>]) -> bool {
    constraints
        .iter()
        .all(|roots| roots.iter().any(|r| canonical.starts_with(r)))
}

/// Build the deny message, naming the real allowed roots so the failure is
/// legible (never a bare sentinel).
fn denied_outside(target: &Path, constraints: &[Vec<PathBuf>]) -> ToolResult {
    let allowed: Vec<String> = constraints
        .iter()
        .flatten()
        .map(|r| r.display().to_string())
        .collect();
    let roots = if allowed.is_empty() {
        "(no writable roots)".to_string()
    } else {
        allowed.join(", ")
    };
    ToolResult::error(format!(
        "path `{}` resolves outside the allowed write root(s): {roots}",
        target.display(),
    ))
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
                json!({
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
                }),
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: WriteFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if parsed.content.len() > self.max_bytes {
            return Ok(ToolResult::error(format!(
                "refusing to write {} bytes; cap is {} (fs.write_file)",
                parsed.content.len(),
                self.max_bytes
            )));
        }

        let resolved = match self.resolve(&parsed.path, ctx.cwd.as_deref(), &ctx.sandbox) {
            Ok(p) => p,
            Err(result) => return Ok(result),
        };

        let basename = resolved
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| resolved.display().to_string());

        let constraints = self.write_constraints(&ctx.sandbox);
        let content_len = parsed.content.len();

        // Race the write against cancel — a coding loop that's been
        // cancelled must not keep mutating the tree. The async builds the full
        // result so a post-canonicalization escape surfaces as an is_error
        // result (recoverable), consistent with the pre-check.
        let write = async {
            let target = if let Some(parent) = resolved.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ToolError::Execute(format!("create parent of `{}`: {e}", resolved.display()))
                })?;
                if constraints.is_empty() {
                    // Unjailed (default) path — write the combined path as-is.
                    resolved.clone()
                } else {
                    // TOCTOU close: the pre-check canonicalized the nearest
                    // EXISTING ancestor, but the write follows the WHOLE path.
                    // A symlink swapped into the (now-materialized) parent
                    // between the pre-check and here would redirect the write
                    // outside the root. Canonicalize the real parent NOW and
                    // re-verify containment, then target `real_parent/basename`
                    // so the bytes can only land inside an allowed root.
                    let real_parent = tokio::fs::canonicalize(parent).await.map_err(|e| {
                        ToolError::Execute(format!(
                            "canonicalize parent of `{}`: {e}",
                            resolved.display()
                        ))
                    })?;
                    if !satisfies(&real_parent, &constraints) {
                        return Ok(denied_outside(&resolved, &constraints));
                    }
                    match resolved.file_name() {
                        Some(name) => real_parent.join(name),
                        None => resolved.clone(),
                    }
                }
            } else {
                resolved.clone()
            };

            tokio::fs::write(&target, parsed.content.as_bytes())
                .await
                .map_err(|e| ToolError::Execute(format!("write `{}`: {e}", target.display())))?;
            Ok(ToolResult::titled_success(
                format!("Wrote {basename}"),
                format!("wrote {content_len} bytes to {}", target.display()),
            ))
        };

        tokio::select! {
            _ = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
            res = write => res,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn ctx(cwd: Option<PathBuf>) -> ToolContext {
        ToolContext {
            cancel: CancellationToken::new(),
            cwd,
            ..Default::default()
        }
    }

    #[test]
    fn name_pins_to_doc_05_convention() {
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
                ctx(Some(dir.clone())),
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
                ctx(Some(dir.clone())),
            )
            .await
            .unwrap();
        assert!(r.is_error, "jail must reject ../ escape");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `ToolContext` carrying an explicit sandbox policy (no `with_root`).
    fn ctx_sandboxed(cwd: Option<PathBuf>, sandbox: SandboxPolicy) -> ToolContext {
        ToolContext {
            cancel: CancellationToken::new(),
            cwd,
            sandbox,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn workspace_write_allows_inside_and_rejects_outside() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sandbox = SandboxPolicy::workspace_write(dir.path());

        // Inside the writable root: allowed.
        let tool = WriteFileTool::new();
        let inside = tool
            .execute(
                json!({ "path": "sub/ok.txt", "content": "hi" }),
                ctx_sandboxed(Some(dir.path().to_path_buf()), sandbox.clone()),
            )
            .await
            .unwrap();
        assert!(!inside.is_error, "write inside writable_root must succeed: {}", inside.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("sub/ok.txt")).unwrap(), "hi");

        // Absolute path outside every writable root: rejected, no file created.
        let escapee = outside.path().join("escape.txt");
        let r = tool
            .execute(
                json!({ "path": escapee.to_str().unwrap(), "content": "x" }),
                ctx_sandboxed(Some(dir.path().to_path_buf()), sandbox),
            )
            .await
            .unwrap();
        assert!(r.is_error, "write outside writable_root must be rejected");
        assert!(r.content.contains("outside the allowed write root"), "got: {}", r.content);
        assert!(!escapee.exists(), "rejected write must not create the file");
    }

    #[tokio::test]
    async fn read_only_rejects_all_writes() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteFileTool::new();
        let r = tool
            .execute(
                json!({ "path": "in_workspace.txt", "content": "x" }),
                ctx_sandboxed(Some(dir.path().to_path_buf()), SandboxPolicy::read_only(false)),
            )
            .await
            .unwrap();
        assert!(r.is_error, "read-only sandbox must reject writes");
        assert!(r.content.contains("read-only"), "got: {}", r.content);
        assert!(!dir.path().join("in_workspace.txt").exists());
    }

    #[tokio::test]
    async fn danger_full_access_imposes_no_new_restriction() {
        // The default policy (DangerFullAccess, empty roots) must leave the
        // historical behaviour intact: an absolute path anywhere writes fine.
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("anywhere.txt");
        let tool = WriteFileTool::new();
        let r = tool
            .execute(
                json!({ "path": target.to_str().unwrap(), "content": "ok" }),
                ctx_sandboxed(None, SandboxPolicy::default()),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "default policy must not restrict: {}", r.content);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "ok");
    }

    #[tokio::test]
    async fn toctou_symlinked_parent_swap_cannot_escape() {
        // A symlinked directory swapped into the path after the ancestor
        // pre-check must not let the write escape: the post-canonicalization
        // re-check resolves the real parent and denies it.
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // `root/link` -> outside (a symlink whose target is OUTSIDE the root).
        let link = root.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        let sandbox = SandboxPolicy::workspace_write(root.path());
        let tool = WriteFileTool::new();
        // `root/link/pwned.txt` — the nearest existing ancestor is `root/link`,
        // which canonicalizes OUTSIDE root, so even the pre-check denies it.
        let r = tool
            .execute(
                json!({ "path": link.join("pwned.txt").to_str().unwrap(), "content": "x" }),
                ctx_sandboxed(Some(root.path().to_path_buf()), sandbox),
            )
            .await
            .unwrap();
        assert!(r.is_error, "write through a symlink pointing outside must be denied");
        assert!(!outside.path().join("pwned.txt").exists(), "no bytes may land outside");
    }
}
