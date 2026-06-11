//! `fs.glob` — find files BY NAME across the work tree (the `fd`
//! equivalent), using ripgrep/fd's `.gitignore`-respecting walker
//! (`ignore`) + fast glob matching (`globset`), linked in.
//!
//! Pairs with [`super::GrepTool`]: grep searches file *contents*, glob
//! locates files by *path pattern*. Same wins as grep — the capability is
//! named in the tool list (so the model uses it instead of `bash.run` +
//! `find`), it is rooted at the jail / cwd and cannot escape, and it skips
//! `.gitignore`d + hidden + binary-heavy noise by default. An empty result
//! is a SUCCESS (`is_error=false`), not a failure.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use tars_types::JsonSchema;

use crate::tool::{Tool, ToolContext, ToolError, ToolResult};

/// Default cap on paths returned per call.
pub const DEFAULT_MAX_RESULTS: usize = 256;

pub struct GlobTool {
    root: Option<PathBuf>,
    max_results: usize,
}

#[derive(Debug, Deserialize)]
struct GlobArgs {
    /// Glob pattern matched against each file's path relative to the
    /// search root (e.g. `*.rs`, `**/*.test.ts`, `src/**/mod.rs`).
    pattern: String,
    /// Directory to search under. Absolute or relative to the working
    /// directory. Defaults to the working directory.
    #[serde(default)]
    path: Option<String>,
}

struct GlobOutcome {
    paths: Vec<String>,
    truncated: bool,
}

impl GlobTool {
    pub fn new() -> Self {
        Self {
            root: None,
            max_results: DEFAULT_MAX_RESULTS,
        }
    }

    /// Constrain the search to `root`. Returns `None` if `root` can't be
    /// canonicalized.
    pub fn with_root(root: impl AsRef<Path>) -> Option<Self> {
        let canonical_root = std::fs::canonicalize(root.as_ref()).ok()?;
        Some(Self {
            root: Some(canonical_root),
            max_results: DEFAULT_MAX_RESULTS,
        })
    }

    pub fn max_results(mut self, n: usize) -> Self {
        self.max_results = n;
        self
    }

    /// Effective jail root = explicit `with_root`, else the per-call
    /// `ctx.cwd` (see [`super::grep`]'s resolve for the rationale — the
    /// structural fix for unbounded `find /` walks).
    fn resolve(&self, input: Option<&str>, cwd: Option<&Path>) -> Result<PathBuf, ToolResult> {
        let raw = Path::new(input.unwrap_or("."));
        let combined = if raw.is_absolute() {
            raw.to_path_buf()
        } else if let Some(cwd) = cwd {
            cwd.join(raw)
        } else {
            raw.to_path_buf()
        };
        let jail = match (&self.root, cwd) {
            (Some(r), _) => r.clone(),
            (None, Some(c)) => std::fs::canonicalize(c).map_err(|e| {
                ToolResult::error(format!("cannot resolve cwd `{}`: {e}", c.display()))
            })?,
            (None, None) => return Ok(combined),
        };
        let canonical = std::fs::canonicalize(&combined).map_err(|e| {
            ToolResult::error(format!("cannot resolve path `{}`: {e}", combined.display()))
        })?;
        if !canonical.starts_with(&jail) {
            return Err(ToolResult::error(format!(
                "path `{}` resolves outside the allowed root `{}`",
                canonical.display(),
                jail.display(),
            )));
        }
        Ok(canonical)
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "fs.glob"
    }

    fn description(&self) -> &str {
        "Find files BY NAME across the working tree matching a glob \
         (e.g. `**/*.rs`, `src/**/mod.rs`) — the fast `fd` equivalent: \
         respects .gitignore, skips hidden files, cannot escape the \
         workspace. Prefer this over `bash.run` with find. Returns matching \
         paths. To search file CONTENTS use fs.grep instead."
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "GlobArgs",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob matched against each file path relative to the search root, e.g. `*.rs` or `**/*.test.ts`." },
                        "path": { "type": "string", "description": "Directory to search under. Relative to the working directory. Defaults to the working directory." }
                    },
                    "required": ["pattern"]
                }),
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: GlobArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let search_root = match self.resolve(parsed.path.as_deref(), ctx.cwd.as_deref()) {
            Ok(p) => p,
            Err(result) => return Ok(result),
        };

        let pattern = parsed.pattern.clone();
        let cap = self.max_results;
        let cancel = ctx.cancel.clone();
        let root_for_blocking = search_root.clone();

        let handle = tokio::task::spawn_blocking(move || {
            run_glob(root_for_blocking, &pattern, cap, &cancel)
        });
        let joined = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = handle => r,
        };

        let outcome = match joined {
            Ok(Ok(o)) => o,
            Ok(Err(GlobError::Cancelled)) => return Err(ToolError::Cancelled),
            Ok(Err(GlobError::BadInput(msg))) => return Ok(ToolResult::titled_error("invalid glob", msg)),
            Err(join_err) => return Err(ToolError::Execute(format!("glob task panicked: {join_err}"))),
        };

        if outcome.paths.is_empty() {
            return Ok(ToolResult::titled_success(
                format!("no files match `{}`", parsed.pattern),
                "(no files)".to_string(),
            ));
        }
        let n = outcome.paths.len();
        let mut body = outcome.paths.join("\n");
        let title = if outcome.truncated {
            body.push_str(&format!("\n\n(truncated at {cap} files — narrow the pattern or path)"));
            format!("{n}+ files (truncated)")
        } else {
            format!("{n} file{}", if n == 1 { "" } else { "s" })
        };
        Ok(ToolResult::titled_success(title, body))
    }
}

enum GlobError {
    Cancelled,
    BadInput(String),
}

fn run_glob(
    search_root: PathBuf,
    pattern: &str,
    max_results: usize,
    cancel: &CancellationToken,
) -> Result<GlobOutcome, GlobError> {
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|e| GlobError::BadInput(format!("invalid glob `{pattern}`: {e}")))?
        .compile_matcher();

    let mut paths: Vec<String> = Vec::new();
    let mut truncated = false;

    // require_git(false): honor .gitignore even when the search root isn't
    // itself a git repo (matches GrepTool + ripgrep/fd behaviour).
    let walker = ignore::WalkBuilder::new(&search_root)
        .require_git(false)
        .build();
    for result in walker {
        if cancel.is_cancelled() {
            return Err(GlobError::Cancelled);
        }
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(&search_root).unwrap_or(path);
        // Match against the relative path AND the bare file name, so both
        // `**/*.rs` (path) and `*.rs` (name) do what the model expects.
        let matches_path = glob.is_match(rel);
        let matches_name = path
            .file_name()
            .map(|n| glob.is_match(Path::new(n)))
            .unwrap_or(false);
        if matches_path || matches_name {
            paths.push(rel.display().to_string());
            if paths.len() >= max_results {
                truncated = true;
                break;
            }
        }
    }
    paths.sort();
    Ok(GlobOutcome { paths, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn write(dir: &Path, name: &str, content: &str) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn name_and_schema() {
        let t = GlobTool::new();
        assert_eq!(t.name(), "fs.glob");
        assert!(t.input_schema().strict);
    }

    #[tokio::test]
    async fn finds_by_extension_recursively() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.rs", "x");
        write(dir.path(), "src/nested/b.rs", "x");
        write(dir.path(), "README.md", "x");
        let tool: Arc<dyn Tool> = Arc::new(GlobTool::new());
        let r = tool
            .execute(
                json!({"pattern": "**/*.rs", "path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("src/a.rs"), "got: {}", r.content);
        assert!(r.content.contains("src/nested/b.rs"));
        assert!(!r.content.contains("README.md"));
    }

    #[tokio::test]
    async fn bare_extension_matches_in_any_dir() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "deep/x.toml", "x");
        let tool: Arc<dyn Tool> = Arc::new(GlobTool::new());
        let r = tool
            .execute(
                json!({"pattern": "*.toml", "path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.content.contains("deep/x.toml"), "got: {}", r.content);
    }

    #[tokio::test]
    async fn no_match_is_success() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "x");
        let tool: Arc<dyn Tool> = Arc::new(GlobTool::new());
        let r = tool
            .execute(
                json!({"pattern": "*.zzz", "path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("no files"));
    }

    #[tokio::test]
    async fn respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "target/\n");
        write(dir.path(), "target/gen.rs", "x");
        write(dir.path(), "src/keep.rs", "x");
        let tool: Arc<dyn Tool> = Arc::new(GlobTool::new());
        let r = tool
            .execute(
                json!({"pattern": "**/*.rs", "path": dir.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.content.contains("src/keep.rs"));
        assert!(!r.content.contains("target/gen.rs"), "gitignore'd: {}", r.content);
    }

    #[tokio::test]
    async fn jail_blocks_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let tool: Arc<dyn Tool> = Arc::new(GlobTool::with_root(dir.path()).unwrap());
        let r = tool
            .execute(
                json!({"pattern": "*", "path": outside.path().to_str().unwrap()}),
                ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("outside the allowed root"));
    }
}
