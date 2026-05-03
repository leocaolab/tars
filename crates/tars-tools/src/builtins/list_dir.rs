//! `fs.list_dir` — list the entries of a directory.
//!
//! ## Why this is the second builtin
//!
//! Pairs naturally with [`super::ReadFileTool`]: the LLM can't read a
//! file it hasn't located. With only `fs.read_file`, the user prompt
//! has to spell out exact paths; adding `fs.list_dir` lets prompts
//! like "summarise the README in this repo" work without prompts
//! containing literal paths.
//!
//! Same safety posture as `fs.read_file`: optional path-jail
//! (canonicalize-then-starts_with), cancel-aware, hard cap on entry
//! count to avoid blowing up the LLM context on a directory with
//! 50 000 files.
//!
//! Output format: one entry per line, `<type><indent><name>[ → target]
//! [bytes]`, sorted lexicographically. Compact enough that a typical
//! source tree listing stays under a few KiB; structured enough that
//! the LLM doesn't have to guess what's a directory vs. a file.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use tars_types::JsonSchema;

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

/// Default cap on entries returned per call.
pub const DEFAULT_MAX_ENTRIES: usize = 256;

/// Description prompt — externalized to `list_dir.txt` so iteration
/// doesn't need a Rust recompile (TODO L-1).
static DESCRIPTION_TRIMMED: LazyLock<String> =
    LazyLock::new(|| include_str!("list_dir.txt").trim_end().to_string());

pub struct ListDirTool {
    /// Optional jail root. Same semantics as `ReadFileTool::with_root`.
    root: Option<PathBuf>,
    /// Cap on entries per call. Truncated entries are flagged in the
    /// result text so the LLM knows the listing is incomplete.
    max_entries: usize,
}

#[derive(Debug, Deserialize)]
struct ListDirArgs {
    /// Directory path. Absolute or relative to [`ToolContext::cwd`]
    /// (or the process cwd when context's cwd is unset).
    path: String,
}

impl ListDirTool {
    /// Construct without a jail. Use only in trusted contexts.
    pub fn new() -> Self {
        Self { root: None, max_entries: DEFAULT_MAX_ENTRIES }
    }

    /// Constrain listings to `root`. Mirrors
    /// [`super::ReadFileTool::with_root`]. Returns `None` if `root`
    /// can't be canonicalized.
    pub fn with_root(root: impl AsRef<Path>) -> Option<Self> {
        let canonical_root = std::fs::canonicalize(root.as_ref()).ok()?;
        Some(Self { root: Some(canonical_root), max_entries: DEFAULT_MAX_ENTRIES })
    }

    /// Override the entry-count cap (default 256). Chainable.
    pub fn max_entries(mut self, n: usize) -> Self {
        self.max_entries = n;
        self
    }

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
            ToolResult::error(format!(
                "cannot resolve path `{}`: {e}",
                combined.display(),
            ))
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

impl Default for ListDirTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "fs.list_dir"
    }

    fn description(&self) -> &str {
        // Externalized to a sibling .txt file (TODO L-1).
        DESCRIPTION_TRIMMED.as_str()
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "ListDirArgs",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path. Absolute or relative to the working directory."
                        }
                    },
                    "required": ["path"]
                }),
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: ListDirArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let resolved = match self.resolve(&parsed.path, ctx.cwd.as_deref()) {
            Ok(p) => p,
            Err(result) => return Ok(result),
        };

        let listing = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = read_dir_capped(&resolved, self.max_entries) => r,
        };

        // Title uses the basename when present (root listings use the
        // full path because the basename would be empty / `.`).
        let display_path = resolved
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| resolved.display().to_string());

        match listing {
            Ok(ListOutcome::Ok { entries, truncated }) => {
                let count = entries.len();
                let body = render_listing(&entries, truncated, self.max_entries);
                let title = if truncated {
                    format!("Listed {display_path}/ ({count}+ entries, truncated)")
                } else if count == 0 {
                    format!("Listed {display_path}/ (empty)")
                } else {
                    format!("Listed {display_path}/ ({count} entries)")
                };
                Ok(ToolResult::titled_success(title, body))
            }
            Ok(ListOutcome::NotFound) => Ok(ToolResult::titled_error(
                format!("{display_path} not found"),
                format!("path `{}` not found", resolved.display()),
            )),
            Ok(ListOutcome::NotDirectory) => Ok(ToolResult::titled_error(
                format!("{display_path} is not a directory"),
                format!(
                    "path `{}` is not a directory; use fs.read_file for files",
                    resolved.display(),
                ),
            )),
            Err(e) => Err(ToolError::Execute(format!(
                "listing `{}`: {e}",
                resolved.display(),
            ))),
        }
    }
}

#[derive(Debug)]
struct Entry {
    name: String,
    kind: EntryKind,
    /// Size in bytes for files; None for directories / symlinks.
    size: Option<u64>,
    /// Symlink target, if applicable.
    target: Option<String>,
}

#[derive(Debug)]
enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

impl EntryKind {
    fn glyph(&self) -> char {
        match self {
            Self::File => 'f',
            Self::Dir => 'd',
            Self::Symlink => 'l',
            Self::Other => '?',
        }
    }
}

enum ListOutcome {
    Ok { entries: Vec<Entry>, truncated: bool },
    NotFound,
    NotDirectory,
}

async fn read_dir_capped(path: &Path, cap: usize) -> std::io::Result<ListOutcome> {
    use tokio::fs;

    let meta = match fs::symlink_metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ListOutcome::NotFound),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return Ok(ListOutcome::NotDirectory);
    }

    let mut rd = fs::read_dir(path).await?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut truncated = false;
    while let Some(child) = rd.next_entry().await? {
        if entries.len() >= cap {
            truncated = true;
            break;
        }
        let name = child.file_name().to_string_lossy().to_string();
        let child_meta = match child.metadata().await {
            Ok(m) => m,
            // Race: entry vanished between read_dir and metadata. Skip.
            Err(_) => continue,
        };
        let symlink_meta = fs::symlink_metadata(child.path()).await.ok();
        let (kind, target) = if symlink_meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
            let t = fs::read_link(child.path()).await.ok().map(|p| p.display().to_string());
            (EntryKind::Symlink, t)
        } else if child_meta.is_dir() {
            (EntryKind::Dir, None)
        } else if child_meta.is_file() {
            (EntryKind::File, None)
        } else {
            (EntryKind::Other, None)
        };
        let size = matches!(kind, EntryKind::File).then_some(child_meta.len());
        entries.push(Entry { name, kind, size, target });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ListOutcome::Ok { entries, truncated })
}

fn render_listing(entries: &[Entry], truncated: bool, cap: usize) -> String {
    if entries.is_empty() {
        return String::from("(empty directory)");
    }
    let mut out = String::with_capacity(entries.len() * 32);
    for e in entries {
        out.push(e.kind.glyph());
        out.push(' ');
        out.push_str(&e.name);
        if let Some(target) = &e.target {
            out.push_str(" -> ");
            out.push_str(target);
        }
        if let Some(size) = e.size {
            out.push_str(&format!(" [{size}]"));
        }
        out.push('\n');
    }
    if truncated {
        out.push_str(&format!(
            "\n(truncated at {cap} entries; directory has more — use a more specific path)\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    use crate::tool::Tool;

    async fn write(path: &Path, content: &[u8]) {
        tokio::fs::write(path, content).await.unwrap();
    }

    #[test]
    fn name_and_description_pin_to_doc_05_convention() {
        let t = ListDirTool::new();
        assert_eq!(t.name(), "fs.list_dir");
        assert!(t.description().to_lowercase().contains("director"));
    }

    #[test]
    fn schema_marks_path_required_and_no_extra_properties() {
        let t = ListDirTool::new();
        let schema = t.input_schema();
        assert!(schema.strict);
        let required: Vec<&str> = schema.schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["path"]);
        assert_eq!(schema.schema["additionalProperties"], json!(false));
    }

    #[tokio::test]
    async fn lists_files_and_subdirs_sorted_with_type_glyphs() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("zeta.txt"), b"hi").await;
        write(&dir.path().join("alpha.rs"), b"fn main() {}").await;
        tokio::fs::create_dir(dir.path().join("subdir")).await.unwrap();

        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::new());
        let r = tool
            .execute(
                json!({"path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        // Sorted: alpha.rs first, subdir, then zeta.txt.
        let lines: Vec<&str> = r.content.lines().collect();
        assert_eq!(lines[0], "f alpha.rs [12]");
        assert_eq!(lines[1], "d subdir");
        assert_eq!(lines[2], "f zeta.txt [2]");
        // L-3: trajectory-readable title with basename + count.
        let basename = dir.path().file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(r.title, format!("Listed {basename}/ (3 entries)"));
    }

    #[tokio::test]
    async fn empty_directory_renders_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::new());
        let r = tool
            .execute(
                json!({"path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content, "(empty directory)");
    }

    #[tokio::test]
    async fn missing_path_yields_is_error_not_hard_error() {
        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::new());
        let r = tool
            .execute(
                json!({"path": "/nonexistent/dir"}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("not found") || r.content.contains("cannot resolve"));
    }

    #[tokio::test]
    async fn pointing_at_a_file_rejects_with_helpful_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not_a_dir.txt");
        write(&path, b"x").await;

        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::new());
        let r = tool
            .execute(
                json!({"path": path.to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("not a directory"));
        assert!(
            r.content.contains("fs.read_file"),
            "should hint at the right tool: {}", r.content,
        );
    }

    #[tokio::test]
    async fn truncates_at_max_entries_and_flags_in_output() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            write(&dir.path().join(format!("f{i:02}.txt")), b"x").await;
        }

        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::new().max_entries(3));
        let r = tool
            .execute(
                json!({"path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let body_lines: Vec<&str> = r.content.lines().filter(|l| l.starts_with('f')).collect();
        assert_eq!(body_lines.len(), 3);
        assert!(r.content.contains("truncated at 3"));
    }

    #[tokio::test]
    async fn jail_blocks_path_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();

        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::with_root(dir.path()).unwrap());
        let r = tool
            .execute(
                json!({"path": outside_dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("outside the allowed root"));
    }

    #[tokio::test]
    async fn invalid_args_yield_typed_invalid_arguments_error() {
        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::new());
        let err = tool
            .execute(json!({"not_path": "x"}), ToolContext::default())
            .await
            .expect_err("should reject malformed args");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn cancelled_before_listing_surfaces_typed_cancelled_error() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolContext { cancel, cwd: None };

        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::new());
        let err = tool
            .execute(json!({"path": dir.path().to_str().unwrap()}), ctx)
            .await
            .expect_err("pre-cancelled should fast-fail");
        assert!(matches!(err, ToolError::Cancelled));
    }
}
