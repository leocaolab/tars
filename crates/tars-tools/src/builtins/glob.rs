//! `fs.glob`: finds files by name under the search root with fd's walker, skipping ignored and
//! hidden files. Where it may search is the read jail's decision (see `read_jail`), as for
//! `fs.grep`. The walker lists directories by path, so a directory swapped for a symlink during
//! the walk can make it list names from outside the root; a name is reported only once it is
//! found to be a regular file below the root, looked up from the root without following a
//! symlink. An entry it cannot read or confirm is skipped, and the result names the first few with
//! why. No match is a success, not an `is_error` result.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use async_trait::async_trait;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use tars_types::JsonSchema;

use super::SkipList;
use super::read_jail::{Reach, ReadJail, ReadPath};
use crate::tool::{Tool, ToolEnv, ToolError, ToolOutput};
use tars_sandbox::{ResolveRootError, canonical_root};

/// Caps the paths one call returns.
pub const DEFAULT_MAX_RESULTS: NonZeroUsize = NonZeroUsize::new(256).expect("256 is not zero");

pub struct GlobTool {
    root: Option<PathBuf>,
    max_results: NonZeroUsize,
}

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

struct GlobOutcome {
    paths: Vec<String>,
    /// Says whether a file past the cap was found, so more files match than are returned.
    truncated: bool,
    skipped: SkipList,
}

impl GlobTool {
    pub fn new() -> Self {
        Self {
            root: None,
            max_results: DEFAULT_MAX_RESULTS,
        }
    }

    /// Confines the search to `root`.
    ///
    /// # Errors
    ///
    /// Fails when `root` cannot be canonicalized.
    pub fn with_root(root: impl AsRef<Path>) -> Result<Self, ResolveRootError> {
        Ok(Self {
            root: Some(canonical_root(root.as_ref())?),
            max_results: DEFAULT_MAX_RESULTS,
        })
    }

    pub fn max_results(mut self, n: NonZeroUsize) -> Self {
        self.max_results = n;
        self
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
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob matched against each file path relative to the search root, e.g. `*.rs` or `**/*.test.ts`." },
                        "path": { "type": "string", "description": "Directory to search under. Relative to the working directory. Defaults to the working directory." }
                    },
                    "required": ["pattern"]
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
        let parsed: GlobArgs = serde_json::from_value(args).map_err(ToolError::InvalidArguments)?;

        let search_root = match ReadJail::of(self.root.as_deref(), env, Reach::Walk)
            .admit(Path::new(parsed.path.as_deref().unwrap_or(".")))
        {
            Ok(p) => p,
            Err(refusal) => return Ok(ToolOutput::error(refusal.to_string())),
        };

        let pattern = parsed.pattern.clone();
        let cap = self.max_results.get();
        let walk_cancel = cancel.clone();

        let handle = tokio::task::spawn_blocking(move || {
            run_glob(&search_root, &pattern, cap, &walk_cancel)
        });
        let joined = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            r = handle => r,
        };

        let outcome = match joined {
            Ok(Ok(o)) => o,
            Ok(Err(GlobError::Cancelled)) => return Err(ToolError::Cancelled),
            Ok(Err(GlobError::BadGlob { pattern, source })) => {
                return Ok(ToolOutput::titled_error(
                    "invalid glob",
                    format!("invalid glob `{pattern}`: {source}"),
                ));
            }
            Ok(Err(GlobError::Root { path, source })) => {
                return Ok(ToolOutput::titled_error(
                    "cannot open the search root",
                    format!("opening `{}`: {source}", path.display()),
                ));
            }
            Err(join_err) => {
                return Err(ToolError::TaskLost {
                    tool: "fs.glob",
                    source: join_err,
                });
            }
        };

        let skipped = outcome.skipped.note("that could not be read");
        if outcome.paths.is_empty() {
            return Ok(ToolOutput::titled_success(
                format!("no files match `{}`", parsed.pattern),
                format!("(no files){skipped}"),
            ));
        }
        let n = outcome.paths.len();
        let mut body = outcome.paths.join("\n");
        let title = if outcome.truncated {
            body.push_str(&format!(
                "\n\n(truncated at {cap} files — narrow the pattern or path)"
            ));
            format!("{n}+ files (truncated)")
        } else {
            format!("{n} file{}", if n == 1 { "" } else { "s" })
        };
        body.push_str(&skipped);
        Ok(ToolOutput::titled_success(title, body))
    }
}

enum GlobError {
    Cancelled,
    BadGlob {
        pattern: String,
        source: globset::Error,
    },
    /// The search root could not be opened to walk from.
    Root {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn run_glob(
    search_root: &ReadPath,
    pattern: &str,
    max_results: usize,
    cancel: &CancellationToken,
) -> Result<GlobOutcome, GlobError> {
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|source| GlobError::BadGlob {
            pattern: pattern.to_owned(),
            source,
        })?
        .compile_matcher();

    let root = search_root.path();
    let opener = search_root.walk().map_err(|source| GlobError::Root {
        path: root.to_path_buf(),
        source,
    })?;
    let mut paths: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut skipped = SkipList::default();

    // `.gitignore` applies even when the search root is not a git repository. Ignore files above
    // the search root are not read, as for `fs.grep`: a worktree under a parent `.gitignore`
    // that lists it would otherwise match nothing.
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .parents(false)
        .build();
    for result in walker {
        if cancel.is_cancelled() {
            return Err(GlobError::Cancelled);
        }
        // An entry the walk cannot read (a directory without permission, a broken link) is
        // skipped and named, so the model knows the paths cover only what could be read.
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => {
                skipped.push(e.to_string());
                continue;
            }
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);
        // The bare name is matched as well as the relative path, so a pattern with no wildcard
        // for the directories (`Cargo.toml`, `a*.rs`) also finds the files of that name below
        // the root.
        let matches_path = glob.is_match(rel);
        let matches_name = path
            .file_name()
            .map(|n| glob.is_match(Path::new(n)))
            .unwrap_or(false);
        if matches_path || matches_name {
            // A name that is not, or cannot be confirmed as, a regular file below the root is
            // skipped and named, as an entry the walk cannot read is.
            match opener.is_file(path) {
                Ok(true) => {}
                Ok(false) => {
                    skipped.push(format!(
                        "`{}`: not a regular file below the search root",
                        rel.display()
                    ));
                    continue;
                }
                Err(e) => {
                    skipped.push(format!("`{}`: {e}", rel.display()));
                    continue;
                }
            }
            // A file past the cap is found, not kept: it is what says the result is cut short.
            if paths.len() == max_results {
                truncated = true;
                break;
            }
            paths.push(rel.display().to_string());
        }
    }
    paths.sort();
    Ok(GlobOutcome {
        paths,
        truncated,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    /// Returns a context whose `cwd` is `dir`, the confinement a rootless tool needs.
    fn in_dir(dir: &Path) -> ToolEnv {
        ToolEnv {
            cwd: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

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
                &in_dir(dir.path()),
                CancellationToken::new(),
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
                &in_dir(dir.path()),
                CancellationToken::new(),
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
                &in_dir(dir.path()),
                CancellationToken::new(),
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
                &in_dir(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.content.contains("src/keep.rs"));
        assert!(
            !r.content.contains("target/gen.rs"),
            "gitignore'd: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn jail_blocks_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let tool: Arc<dyn Tool> = Arc::new(GlobTool::with_root(dir.path()).unwrap());
        let r = tool
            .execute(
                json!({"pattern": "*", "path": outside.path().to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("outside the allowed root"));
    }

    #[tokio::test]
    async fn a_rootless_search_without_a_cwd_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "x");
        let r = GlobTool::new()
            .execute(
                json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
    }

    #[tokio::test]
    async fn searches_a_root_that_a_parent_gitignore_marks_ignored() {
        let repo = tempfile::tempdir().unwrap();
        write(repo.path(), ".gitignore", ".arc/\n");
        let worktree = repo.path().join(".arc/worktrees/fix-X");
        write(&worktree, "src/lib.rs", "x");
        let r = GlobTool::new()
            .execute(
                json!({"pattern": "**/*.rs", "path": worktree.to_str().unwrap()}),
                &in_dir(&worktree),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.content.contains("src/lib.rs"), "got: {}", r.content);
    }

    #[tokio::test]
    async fn a_bare_name_finds_the_files_of_that_name_below_the_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/one/Cargo.toml", "x");
        let r = GlobTool::new()
            .execute(
                json!({"pattern": "Cargo.toml", "path": dir.path().to_str().unwrap()}),
                &in_dir(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            r.content.contains("crates/one/Cargo.toml"),
            "got: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn exactly_cap_files_are_all_there_is_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "only.rs", "");
        let r = GlobTool::new()
            .max_results(NonZeroUsize::new(1).unwrap())
            .execute(
                json!({"pattern": "*.rs"}),
                &in_dir(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(r.title.as_deref(), Some("1 file"), "{}", r.content);
        assert!(!r.content.contains("truncated"), "{}", r.content);
    }

    #[tokio::test]
    async fn a_file_past_the_cap_says_truncated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", "");
        write(dir.path(), "b.rs", "");
        let r = GlobTool::new()
            .max_results(NonZeroUsize::new(1).unwrap())
            .execute(
                json!({"pattern": "*.rs"}),
                &in_dir(dir.path()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.title.as_deref(),
            Some("1+ files (truncated)"),
            "{}",
            r.content
        );
    }
}
