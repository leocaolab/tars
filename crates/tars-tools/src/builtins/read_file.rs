//! `fs.read_file` — read a UTF-8 text file.
//!
//! ## Why this is the first builtin
//!
//! Read-only (no rollback story to design before shipping; Saga
//! compensation is future work), useful in tests, exercises every
//! [`Tool`] trait responsibility (schema-driven args, async I/O,
//! cancellation, error handling, size-cap, jail). Once it works the
//! pattern for writing additional read-only tools (`fs.list_dir`,
//! `git.fetch_pr_diff`, `web.fetch`) becomes mechanical.
//!
//! ## Safety
//!
//! - **Path jail**: optional. When constructed via [`ReadFileTool::with_root`],
//!   any path that resolves outside `root` is rejected. When constructed
//!   via [`ReadFileTool::new`], no jail — the tool reads anywhere the
//!   process can. Production callers should use `with_root` to constrain
//!   the LLM to a specific repo / workspace.
//! - **Symlink handling**: paths are canonicalized via
//!   [`std::fs::canonicalize`] before the jail check, so a symlink
//!   pointing outside the root is also rejected. The canonicalization
//!   itself only fails when the file doesn't exist, which we surface
//!   as a `not found` `is_error` result rather than a hard error.
//! - **Size cap**: hard limit on bytes read. Default 256 KiB — enough
//!   for typical source files, small enough that a worst-case "read
//!   /var/log/..." doesn't blow up the LLM context.
//! - **UTF-8 only**: binary files surface as an `is_error` result;
//!   the model gets a clean signal to try a different path rather
//!   than hallucinating content from byte garbage.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use tars_types::JsonSchema;

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

/// Default max bytes read by `fs.read_file`. ~256 KiB.
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024;

/// Description prompt — externalized to `read_file.txt` so iteration
/// doesn't need a Rust recompile (TODO L-1). Loaded once at first
/// access; trailing newline is trimmed because the file naturally
/// ends with one and that's noise to consumers.
static DESCRIPTION_TRIMMED: LazyLock<String> =
    LazyLock::new(|| include_str!("read_file.txt").trim_end().to_string());

pub struct ReadFileTool {
    /// If set, all paths must resolve inside this directory after
    /// canonicalization. `None` = no jail (use only in trusted
    /// contexts).
    root: Option<PathBuf>,
    /// Hard cap on bytes read per invocation.
    max_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    /// File path. Absolute or relative to [`ToolContext::cwd`] (or
    /// the process cwd when context's cwd is unset).
    path: String,
}

impl ReadFileTool {
    /// Construct without a jail. The tool can read anywhere the
    /// process can. Use only in trusted contexts (CLI run by the
    /// user against their own filesystem). Tests use this.
    pub fn new() -> Self {
        Self { root: None, max_bytes: DEFAULT_MAX_BYTES }
    }

    /// Constrain reads to `root`. Any path that resolves outside
    /// `root` after canonicalization is rejected with an
    /// `is_error` result (not a hard `ToolError` — the model should
    /// try a different path rather than abort the loop).
    ///
    /// `root` itself is canonicalized eagerly so symlinks-to-the-root
    /// behave the same as the root directly. If `root` doesn't exist
    /// or can't be canonicalized, returns `None` — caller decides
    /// how to surface the misconfiguration.
    pub fn with_root(root: impl AsRef<Path>) -> Option<Self> {
        let canonical_root = std::fs::canonicalize(root.as_ref()).ok()?;
        Some(Self { root: Some(canonical_root), max_bytes: DEFAULT_MAX_BYTES })
    }

    /// Override the max-bytes cap (default 256 KiB). Chainable.
    pub fn max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = n;
        self
    }

    /// Resolve an input path against the optional `cwd` hint, then
    /// (if jail is enabled) canonicalize and verify it sits inside
    /// `root`. Returns the path to actually open.
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

        // Jail enforcement requires canonicalization, which fails if
        // the file doesn't exist. We surface that as the same
        // "not found" error the actual read would, so callers see
        // one consistent message.
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

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "fs.read_file"
    }

    fn description(&self) -> &str {
        // Externalized to a sibling .txt file (TODO L-1) so prompt
        // iteration doesn't need a Rust recompile + commit. The
        // trailing newline from the .txt is harmless to the LLM but
        // would clutter equality assertions; trim it once.
        DESCRIPTION_TRIMMED.as_str()
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "ReadFileArgs",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path. Absolute or relative to the working directory."
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
        let parsed: ReadFileArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let resolved = match self.resolve(&parsed.path, ctx.cwd.as_deref()) {
            Ok(p) => p,
            Err(result) => return Ok(result),
        };
        // Use the basename for titles — full path is in `content` for
        // the LLM; the title is for human eyeballs scanning trajectory
        // logs and wants compact identifiers.
        let basename = resolved
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| resolved.display().to_string());

        // Race the actual read against cancel — fast-fail rather
        // than letting an upstream Drop wait for the file syscall.
        let bytes = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = read_capped(&resolved, self.max_bytes) => r,
        };

        match bytes {
            Ok(ReadOutcome::Ok(content)) => {
                let title = format!("Read {basename} ({} bytes)", content.len());
                Ok(ToolResult::titled_success(title, content))
            }
            Ok(ReadOutcome::TooLarge { size }) => Ok(ToolResult::titled_error(
                format!("{basename} too large ({size} bytes)"),
                format!(
                    "file `{}` is {size} bytes, exceeds the {} byte cap; \
                     read a more specific path or increase the cap",
                    resolved.display(),
                    self.max_bytes,
                ),
            )),
            Ok(ReadOutcome::NotUtf8) => Ok(ToolResult::titled_error(
                format!("{basename} is not UTF-8"),
                format!(
                    "file `{}` is not valid UTF-8 (binary?); fs.read_file only \
                     returns text",
                    resolved.display(),
                ),
            )),
            Ok(ReadOutcome::NotFound) => Ok(ToolResult::titled_error(
                format!("{basename} not found"),
                format!("file `{}` not found", resolved.display()),
            )),
            Err(e) => Err(ToolError::Execute(format!(
                "reading `{}`: {e}",
                resolved.display(),
            ))),
        }
    }
}

enum ReadOutcome {
    Ok(String),
    TooLarge { size: u64 },
    NotUtf8,
    NotFound,
}

async fn read_capped(path: &Path, cap: u64) -> std::io::Result<ReadOutcome> {
    use tokio::fs;
    use tokio::io::AsyncReadExt;

    // Stat first so we can reject too-large files before allocating
    // the Vec; saves wasted syscalls on `read /var/log/messages`.
    let meta = match fs::metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ReadOutcome::NotFound),
        Err(e) => return Err(e),
    };
    if meta.len() > cap {
        return Ok(ReadOutcome::TooLarge { size: meta.len() });
    }

    let file = fs::File::open(path).await?;
    let mut buf = Vec::with_capacity(meta.len() as usize);
    // `take(cap)` is a belt-and-suspenders against a file growing
    // between the metadata check and the actual read.
    file.take(cap).read_to_end(&mut buf).await?;
    match String::from_utf8(buf) {
        Ok(s) => Ok(ReadOutcome::Ok(s)),
        Err(_) => Ok(ReadOutcome::NotUtf8),
    }
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
        let t = ReadFileTool::new();
        assert_eq!(t.name(), "fs.read_file");
        assert!(t.description().to_lowercase().contains("read"));
    }

    #[test]
    fn schema_marks_path_required_and_no_extra_properties() {
        let t = ReadFileTool::new();
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
    async fn read_file_happy_path_returns_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        write(&path, b"hello world").await;

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
        let r = tool
            .execute(
                json!({"path": path.to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content, "hello world");
        // L-3: trajectory-readable title with basename + size.
        assert_eq!(r.title, "Read hello.txt (11 bytes)");
    }

    #[tokio::test]
    async fn missing_file_yields_is_error_not_hard_error() {
        // Hard errors break the agent loop; an `is_error` Ok lets
        // the LLM try a different path on the next turn.
        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
        let r = tool
            .execute(
                json!({"path": "/nonexistent/path/to/nothing.txt"}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("not found") || r.content.contains("cannot resolve"));
    }

    #[tokio::test]
    async fn binary_file_rejects_with_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        write(&path, &[0xff, 0xfe, 0x00, 0x80]).await;

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
        let r = tool
            .execute(
                json!({"path": path.to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("UTF-8"), "should mention UTF-8: {}", r.content);
    }

    #[tokio::test]
    async fn over_cap_file_rejects_with_size_in_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        write(&path, &vec![b'a'; 1024]).await;

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new().max_bytes(100));
        let r = tool
            .execute(
                json!({"path": path.to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("1024"), "should report file size: {}", r.content);
        assert!(r.content.contains("100"), "should report cap: {}", r.content);
    }

    #[tokio::test]
    async fn jail_blocks_path_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("ok.txt");
        write(&inside, b"inside").await;

        // Create something outside the jail root.
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("naughty.txt");
        write(&outside, b"escaped").await;

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::with_root(dir.path()).unwrap());

        // Inside is fine.
        let r = tool
            .execute(
                json!({"path": inside.to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content, "inside");

        // Outside is blocked.
        let r = tool
            .execute(
                json!({"path": outside.to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("outside the allowed root"));
    }

    #[tokio::test]
    async fn jail_resolves_relative_paths_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rel.txt");
        write(&path, b"rel ok").await;

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::with_root(dir.path()).unwrap());
        let ctx = ToolContext { cwd: Some(dir.path().to_path_buf()), ..Default::default() };
        let r = tool
            .execute(json!({"path": "rel.txt"}), ctx)
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content, "rel ok");
    }

    #[tokio::test]
    async fn invalid_args_yield_typed_invalid_arguments_error() {
        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
        let err = tool
            .execute(json!({"not_path": "x"}), ToolContext::default())
            .await
            .expect_err("should reject malformed args");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn cancelled_before_read_surfaces_typed_cancelled_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.txt");
        write(&path, b"hi").await;

        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolContext { cancel, cwd: None };

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
        let err = tool
            .execute(json!({"path": path.to_str().unwrap()}), ctx)
            .await
            .expect_err("pre-cancelled should fast-fail");
        assert!(matches!(err, ToolError::Cancelled));
    }
}
