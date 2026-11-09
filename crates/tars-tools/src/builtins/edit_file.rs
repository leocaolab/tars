//! `fs.edit_file`: replaces an exact substring in an existing file, so a small change does not
//! make the model send the whole file back. `old_string` must occur exactly once unless
//! `replace_all` is set, so a stale match cannot change an unrelated line.
//!
//! Where it may write is the write jail's decision, as for `fs.write_file`: under the tool's
//! root and the sandbox's writable roots, never under `ReadOnly` or a protected git directory.
//! A confined edit resolves the path whole first, so a symlink to a file outside is refused. The
//! file is opened once, without blocking, and read and rewritten through that one handle; only a
//! regular file of at most [`MAX_EDIT_BYTES`] is edited.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use serde::Deserialize;

use tars_types::JsonSchema;

use super::write_jail::{WriteAccess, WriteError, WriteJail, WriteTarget, replace_contents};
use crate::tool::{FsOp, Tool, ToolEnv, ToolError, ToolOutput};
use tars_sandbox::{ResolveRootError, canonical_root};

/// Caps the size of a file an edit reads at `fs.write_file`'s default cap on one write, since an
/// edit writes the whole file back.
const MAX_EDIT_BYTES: u64 = super::write_file::DEFAULT_MAX_BYTES.get() as u64;

pub struct EditFileTool {
    /// Holds the canonical directory a resolved path must sit inside, when set.
    root: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct EditFileArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl EditFileTool {
    pub fn new() -> Self {
        Self { root: None }
    }

    /// Confines edits to `root`, refusing a path outside it with an `is_error` result.
    ///
    /// # Errors
    ///
    /// Fails when `root` cannot be canonicalized.
    pub fn with_root(root: impl AsRef<Path>) -> Result<Self, ResolveRootError> {
        Ok(Self {
            root: Some(canonical_root(root.as_ref())?),
        })
    }
}

/// Says why `old_string` does not name one place to edit.
#[derive(Debug, PartialEq, Eq)]
enum MatchError {
    NotFound,
    Ambiguous { count: usize },
}

/// Returns whether `count` occurrences of `old_string` can be replaced: at least one, and only
/// one unless `replace_all`.
fn check_matches(count: usize, replace_all: bool) -> Result<(), MatchError> {
    match count {
        0 => Err(MatchError::NotFound),
        1 => Ok(()),
        _ if replace_all => Ok(()),
        count => Err(MatchError::Ambiguous { count }),
    }
}

/// Says why an edit did not happen: the jail or the filesystem refused it, the file is not one
/// it edits, or `old_string` did not name one place.
enum EditError {
    Write(WriteError),
    TooLarge { size: u64 },
    NotUtf8,
    Match(MatchError),
}

/// Replaces `old_string` in the file `target` names, reading and writing it through one handle.
/// It blocks on the filesystem, so it runs on the blocking pool. Returns how many occurrences it
/// replaced.
fn edit(target: &WriteTarget, args: &EditFileArgs) -> Result<usize, EditError> {
    use std::io::Read;

    let io = |op, source| {
        EditError::Write(WriteError::Io {
            op,
            path: target.path(),
            source,
        })
    };
    let mut file = target.open(WriteAccess::Edit)?;
    let size = file.metadata().map_err(|e| io(FsOp::Inspect, e))?.len();
    if size > MAX_EDIT_BYTES {
        return Err(EditError::TooLarge { size });
    }
    let mut bytes = Vec::new();
    // `take` bounds a file that grew after the size check.
    (&mut file)
        .take(MAX_EDIT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io(FsOp::Read, e))?;
    let size = bytes.len() as u64;
    if size > MAX_EDIT_BYTES {
        return Err(EditError::TooLarge { size });
    }
    let body = String::from_utf8(bytes).map_err(|_| EditError::NotUtf8)?;
    let count = body.matches(&args.old_string).count();
    check_matches(count, args.replace_all).map_err(EditError::Match)?;
    let updated = if args.replace_all {
        body.replace(&args.old_string, &args.new_string)
    } else {
        body.replacen(&args.old_string, &args.new_string, 1)
    };
    replace_contents(&mut file, updated.as_bytes()).map_err(|e| io(FsOp::Write, e))?;
    Ok(count)
}

impl From<WriteError> for EditError {
    fn from(e: WriteError) -> Self {
        Self::Write(e)
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
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": { "type": "string", "description": "File path, absolute or relative to the working directory." },
                        "old_string": { "type": "string", "description": "Exact substring to find (must be unique unless replace_all)." },
                        "new_string": { "type": "string", "description": "Replacement text." },
                        "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of requiring exactly one." }
                    },
                    "required": ["path", "old_string", "new_string"]
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
        let parsed: EditFileArgs =
            serde_json::from_value(args).map_err(ToolError::InvalidArguments)?;

        if parsed.old_string.is_empty() {
            return Ok(ToolOutput::error(
                "old_string must not be empty (fs.edit_file)",
            ));
        }

        let admitted = WriteJail::of(self.root.as_deref(), env)
            .and_then(|jail| jail.admit_existing(Path::new(&parsed.path)));
        let target = match admitted {
            Ok(t) => t,
            Err(e) => return e.into_tool(),
        };
        let path = target.path();
        let basename = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        let run = tokio::task::spawn_blocking(move || edit(&target, &parsed));
        let joined = tokio::select! {
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            r = run => r,
        };
        let edited = match joined {
            Ok(r) => r,
            Err(source) => {
                return Err(ToolError::TaskLost {
                    tool: "fs.edit_file",
                    source,
                });
            }
        };
        match edited {
            Ok(count) => Ok(ToolOutput::titled_success(
                format!("Edited {basename}"),
                format!("replaced {count} occurrence(s) in {}", path.display()),
            )),
            Err(EditError::Write(w)) => w.into_tool(),
            Err(EditError::TooLarge { size }) => Ok(ToolOutput::error(format!(
                "{basename} is {size} bytes, over the {MAX_EDIT_BYTES} byte cap an edit reads; \
                 nothing edited"
            ))),
            Err(EditError::NotUtf8) => Ok(ToolOutput::error(format!(
                "{basename} is not valid UTF-8 (binary?); fs.edit_file edits text, nothing edited"
            ))),
            Err(EditError::Match(MatchError::NotFound)) => Ok(ToolOutput::error(format!(
                "old_string not found in {basename} — nothing edited"
            ))),
            Err(EditError::Match(MatchError::Ambiguous { count })) => {
                Ok(ToolOutput::error(format!(
                    "old_string occurs {count}× in {basename}; pass replace_all or make it unique"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn ctx(cwd: Option<PathBuf>) -> ToolEnv {
        ToolEnv {
            cwd,
            ..Default::default()
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
                &ctx(Some(dir.clone())),
                CancellationToken::new(),
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
                &ctx(Some(dir.clone())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "ambiguous match must be rejected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sandboxed(cwd: &Path, sandbox: crate::SandboxPolicy) -> ToolEnv {
        ToolEnv {
            cwd: Some(cwd.to_path_buf()),
            sandbox,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_read_only_sandbox_refuses_an_edit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "old\n").unwrap();
        let r = EditFileTool::new()
            .execute(
                json!({ "path": "a.rs", "old_string": "old", "new_string": "new" }),
                &sandboxed(dir.path(), crate::SandboxPolicy::read_only(false)),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("read-only"), "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "old\n"
        );
    }

    #[tokio::test]
    async fn a_workspace_write_sandbox_refuses_an_edit_outside_its_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("a.rs");
        std::fs::write(&victim, "old\n").unwrap();
        let r = EditFileTool::new()
            .execute(
                json!({ "path": victim.to_str().unwrap(), "old_string": "old", "new_string": "new" }), &sandboxed(root.path(), crate::SandboxPolicy::workspace_write(root.path())), CancellationToken::new())
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(
            r.content.contains("outside the allowed write root"),
            "{}",
            r.content
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "old\n");
    }

    #[tokio::test]
    async fn a_symlink_to_a_file_outside_the_sandbox_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("a.rs");
        std::fs::write(&victim, "old\n").unwrap();
        std::os::unix::fs::symlink(&victim, root.path().join("link.rs")).unwrap();
        let r = EditFileTool::new()
            .execute(
                json!({ "path": "link.rs", "old_string": "old", "new_string": "new" }),
                &sandboxed(
                    root.path(),
                    crate::SandboxPolicy::workspace_write(root.path()),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "old\n");
    }

    #[tokio::test]
    async fn a_workspace_write_sandbox_allows_an_edit_inside_its_roots() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "old\n").unwrap();
        let r = EditFileTool::new()
            .execute(
                json!({ "path": "a.rs", "old_string": "old", "new_string": "new" }),
                &sandboxed(
                    root.path(),
                    crate::SandboxPolicy::workspace_write(root.path()),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.rs")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn one_occurrence_is_replaced() {
        assert_eq!(check_matches(1, false), Ok(()));
    }

    #[test]
    fn no_occurrence_is_not_found() {
        assert_eq!(check_matches(0, true), Err(MatchError::NotFound));
    }

    #[test]
    fn two_occurrences_need_replace_all() {
        assert_eq!(
            check_matches(2, false),
            Err(MatchError::Ambiguous { count: 2 })
        );
        assert_eq!(check_matches(2, true), Ok(()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hard_link_to_a_file_outside_the_root_is_not_edited() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = base.path().join("outside.rs");
        std::fs::write(&outside, "old\n").unwrap();
        std::fs::hard_link(&outside, root.join("leaf.rs")).unwrap();
        let r = EditFileTool::with_root(&root)
            .unwrap()
            .execute(
                json!({ "path": "leaf.rs", "old_string": "old", "new_string": "new" }),
                &sandboxed(&root, crate::SandboxPolicy::workspace_write(&root)),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("hard link"), "{}", r.content);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "old\n");
    }

    #[tokio::test]
    async fn a_missing_file_is_the_models_to_fix_with_or_without_a_jail() {
        let dir = tempfile::tempdir().unwrap();
        let args = json!({ "path": "absent.rs", "old_string": "a", "new_string": "b" });
        let unconfined = EditFileTool::new()
            .execute(
                args.clone(),
                &ctx(Some(dir.path().to_path_buf())),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(unconfined.is_error, "{}", unconfined.content);
        let confined = EditFileTool::new()
            .execute(
                args,
                &sandboxed(
                    dir.path(),
                    crate::SandboxPolicy::workspace_write(dir.path()),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(confined.is_error, "{}", confined.content);
    }
}
