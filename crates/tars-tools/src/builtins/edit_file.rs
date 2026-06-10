//! `fs.edit_file` — replace an exact substring in an existing file.
//!
//! The in-place edit half of the filesystem builtins. For a small change
//! to a large file this beats `fs.write_file` (the model doesn't have to
//! echo the whole file back). Semantics mirror the Claude Code `Edit`
//! tool: `old_string` must occur EXACTLY ONCE (unless `replace_all`), so
//! the edit is unambiguous and a stale match can't silently corrupt an
//! unrelated line.
//!
//! Side effect: **Reversible** (Doc 04 §4.4).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use tars_types::JsonSchema;

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

pub struct EditFileTool {
    /// If set, the resolved path must sit inside this directory.
    root: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct EditFileArgs {
    /// File path. Absolute or relative to [`ToolContext::cwd`].
    path: String,
    /// Exact substring to find. Must occur once unless `replace_all`.
    old_string: String,
    /// Replacement text.
    new_string: String,
    /// Replace every occurrence instead of requiring exactly one.
    #[serde(default)]
    replace_all: bool,
}

impl EditFileTool {
    pub fn new() -> Self {
        Self { root: None }
    }

    pub fn with_root(root: impl AsRef<Path>) -> Option<Self> {
        let canonical_root = std::fs::canonicalize(root.as_ref()).ok()?;
        Some(Self {
            root: Some(canonical_root),
        })
    }

    /// Resolve + jail. The file must already exist (edit, not create), so
    /// we canonicalize the leaf directly — same as `fs.read_file`.
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
        let canonical = std::fs::canonicalize(&combined).map_err(|e| {
            ToolResult::error(format!("cannot resolve `{}`: {e}", combined.display()))
        })?;
        if !canonical.starts_with(root) {
            return Err(ToolResult::error(format!(
                "path `{}` resolves outside the allowed root `{}`",
                canonical.display(),
                root.display(),
            )));
        }
        Ok(canonical)
    }
}

impl Default for EditFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "fs.edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact substring in an existing file. `old_string` must \
         occur exactly once (set replace_all to change every occurrence). \
         Prefer this over fs.write_file for a small change to a large file."
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "EditFileArgs",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": { "type": "string", "description": "File path, absolute or relative to the working directory." },
                        "old_string": { "type": "string", "description": "Exact substring to find (must be unique unless replace_all)." },
                        "new_string": { "type": "string", "description": "Replacement text." },
                        "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of requiring exactly one." }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: EditFileArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        if parsed.old_string.is_empty() {
            return Ok(ToolResult::error(
                "old_string must not be empty (fs.edit_file)",
            ));
        }

        let resolved = match self.resolve(&parsed.path, ctx.cwd.as_deref()) {
            Ok(p) => p,
            Err(result) => return Ok(result),
        };
        let basename = resolved
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| resolved.display().to_string());

        let edit = async {
            let body = tokio::fs::read_to_string(&resolved)
                .await
                .map_err(|e| ToolError::Execute(format!("read `{}`: {e}", resolved.display())))?;
            let count = body.matches(&parsed.old_string).count();
            if count == 0 {
                return Ok(ToolResult::error(format!(
                    "old_string not found in {basename} — nothing edited"
                )));
            }
            if count > 1 && !parsed.replace_all {
                return Ok(ToolResult::error(format!(
                    "old_string occurs {count}× in {basename}; pass replace_all or make it unique"
                )));
            }
            let updated = if parsed.replace_all {
                body.replace(&parsed.old_string, &parsed.new_string)
            } else {
                body.replacen(&parsed.old_string, &parsed.new_string, 1)
            };
            tokio::fs::write(&resolved, updated.as_bytes())
                .await
                .map_err(|e| ToolError::Execute(format!("write `{}`: {e}", resolved.display())))?;
            Ok(ToolResult::titled_success(
                format!("Edited {basename}"),
                format!("replaced {count} occurrence(s) in {}", resolved.display()),
            ))
        };

        tokio::select! {
            _ = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
            res = edit => res,
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
        }
    }

    #[tokio::test]
    async fn replaces_unique_substring() {
        let dir = std::env::temp_dir().join(format!("tars_ef_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "let x = old();\n").unwrap();
        let r = EditFileTool::new()
            .execute(
                json!({ "path": "a.rs", "old_string": "old()", "new_string": "new()" }),
                ctx(Some(dir.clone())),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs")).unwrap(),
            "let x = new();\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_ambiguous_match_without_replace_all() {
        let dir = std::env::temp_dir().join(format!("tars_ef_amb_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "x x\n").unwrap();
        let r = EditFileTool::new()
            .execute(
                json!({ "path": "a.rs", "old_string": "x", "new_string": "y" }),
                ctx(Some(dir.clone())),
            )
            .await
            .unwrap();
        assert!(r.is_error, "ambiguous match must be rejected");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
