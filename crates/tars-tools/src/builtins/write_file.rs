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

    /// Resolve an input path against `cwd`, then (if jailed) verify it sits
    /// inside `root`. Unlike a read, the target may not exist yet, so the
    /// jail check canonicalizes the nearest EXISTING ancestor and requires
    /// THAT to be inside the root.
    fn resolve(&self, input: &str, cwd: Option<&Path>) -> Result<PathBuf, ToolResult> {
        let raw = Path::new(input);
        let combined = if raw.is_absolute() {
            raw.to_path_buf()
        } else if let Some(cwd) = cwd {
            cwd.join(raw)
        } else {
            raw.to_path_buf()
        };

        let Some(root) = &self.root else {
            return Ok(combined);
        };

        // Walk up to the nearest existing ancestor and canonicalize it —
        // that defeats `..` traversal and symlink escapes even when the
        // leaf file doesn't exist yet.
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
        if !canonical_existing.starts_with(root) {
            return Err(ToolResult::error(format!(
                "path `{}` resolves outside the allowed root `{}`",
                combined.display(),
                root.display(),
            )));
        }
        Ok(combined)
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

        let resolved = match self.resolve(&parsed.path, ctx.cwd.as_deref()) {
            Ok(p) => p,
            Err(result) => return Ok(result),
        };

        let basename = resolved
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| resolved.display().to_string());

        // Race the write against cancel — a coding loop that's been
        // cancelled must not keep mutating the tree.
        let write = async {
            if let Some(parent) = resolved.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ToolError::Execute(format!("create parent of `{}`: {e}", resolved.display()))
                })?;
            }
            tokio::fs::write(&resolved, parsed.content.as_bytes())
                .await
                .map_err(|e| ToolError::Execute(format!("write `{}`: {e}", resolved.display())))
        };

        tokio::select! {
            _ = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
            res = write => {
                res?;
                Ok(ToolResult::titled_success(
                    format!("Wrote {basename}"),
                    format!("wrote {} bytes to {}", parsed.content.len(), resolved.display()),
                ))
            }
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
}
