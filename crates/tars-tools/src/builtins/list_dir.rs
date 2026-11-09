//! `fs.list_dir`: lists a directory one entry per line as `<f|d|l|?> <name>[ -> target]
//! [ [bytes]]`, so the model can find a file before it reads it. The entries shown are sorted by
//! name. Past the cap, which entries are kept follows the order the directory yields them, not
//! the alphabet, and the listing says it is truncated. Where it may list is the read jail's
//! decision (see `read_jail`); a confined directory is then opened without following a symlink
//! at any component, and its entries are read and looked at through that handle, so one on the
//! way swapped for a symlink after the check refuses the listing instead of showing names from
//! outside the roots.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};
use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use serde::Deserialize;

use tars_types::JsonSchema;

use super::fs_open::{self, Entry, EntryKind};
use super::model_can_fix;
use super::read_jail::{OpenError, Reach, ReadJail, ReadPath};
use crate::tool::{FsOp, Tool, ToolEnv, ToolError, ToolOutput};
use tars_sandbox::{ResolveRootError, canonical_root};

/// Caps the entries one call returns.
pub const DEFAULT_MAX_ENTRIES: NonZeroUsize = NonZeroUsize::new(256).expect("256 is not zero");

static DESCRIPTION_TRIMMED: LazyLock<String> =
    LazyLock::new(|| include_str!("list_dir.txt").trim_end().to_string());

pub struct ListDirTool {
    /// Holds the directory a resolved path must sit inside, when set.
    root: Option<PathBuf>,
    max_entries: NonZeroUsize,
}

#[derive(Debug, Deserialize)]
struct ListDirArgs {
    path: String,
}

impl ListDirTool {
    /// Returns a tool with no root of its own; it lists anywhere the process can unless
    /// [`ToolEnv::sandbox`] confines it to `cwd` and the readable roots.
    pub fn new() -> Self {
        Self {
            root: None,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    /// Confines listings to `root` and the call's readable roots.
    ///
    /// # Errors
    ///
    /// Fails when `root` cannot be canonicalized.
    pub fn with_root(root: impl AsRef<Path>) -> Result<Self, ResolveRootError> {
        Ok(Self {
            root: Some(canonical_root(root.as_ref())?),
            max_entries: DEFAULT_MAX_ENTRIES,
        })
    }

    /// Sets the entry cap; the default is 256.
    pub fn max_entries(mut self, n: NonZeroUsize) -> Self {
        self.max_entries = n;
        self
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
        DESCRIPTION_TRIMMED.as_str()
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "ListDirArgs",
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path. Absolute or relative to the working directory."
                        }
                    },
                    "required": ["path"]
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
        let parsed: ListDirArgs =
            serde_json::from_value(args).map_err(ToolError::InvalidArguments)?;

        let admitted = match ReadJail::of(self.root.as_deref(), env, Reach::Path)
            .admit(Path::new(&parsed.path))
        {
            Ok(p) => p,
            Err(refusal) => return Ok(ToolOutput::error(refusal.to_string())),
        };
        let resolved = admitted.path().to_path_buf();

        let cap = self.max_entries.get();
        let list = tokio::task::spawn_blocking(move || list(&admitted, cap));
        let listing = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            r = list => match r {
                Ok(r) => r,
                Err(source) => return Err(ToolError::TaskLost {
                tool: "fs.list_dir",
                source,
            }),
            },
        };

        // `/` has no basename, so its title uses the full path.
        let display_path = resolved
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| resolved.display().to_string());

        match listing {
            Ok(Listing { entries, truncated }) => {
                let count = entries.len();
                let body = render_listing(&entries, truncated, self.max_entries.get());
                let title = if truncated {
                    format!("Listed {display_path}/ ({count}+ entries, truncated)")
                } else if count == 0 {
                    format!("Listed {display_path}/ (empty)")
                } else {
                    format!("Listed {display_path}/ ({count} entries)")
                };
                Ok(ToolOutput::titled_success(title, body))
            }
            Err(ListDirError::Open(OpenError::NotFound)) => Ok(ToolOutput::titled_error(
                format!("{display_path} not found"),
                format!("path `{}` not found", resolved.display()),
            )),
            Err(ListDirError::Open(OpenError::Symlink)) => Ok(ToolOutput::titled_error(
                format!("{display_path} is a symbolic link"),
                format!(
                    "`{}`, or a directory on the way to it, became a symbolic link after it was \
                     checked; a confined listing does not follow it",
                    resolved.display(),
                ),
            )),
            Err(ListDirError::Open(OpenError::WrongKind)) => Ok(ToolOutput::titled_error(
                format!("{display_path} is not a directory"),
                format!(
                    "path `{}` is not a directory; use fs.read_file for files",
                    resolved.display(),
                ),
            )),
            Err(ListDirError::Open(OpenError::Io(e)) | ListDirError::Io(e))
                if model_can_fix(e.kind()) =>
            {
                Ok(ToolOutput::titled_error(
                    format!("{display_path} cannot be listed"),
                    format!("listing `{}`: {e}", resolved.display()),
                ))
            }
            Err(ListDirError::Open(OpenError::Io(source)) | ListDirError::Io(source)) => {
                Err(ToolError::Io {
                    op: FsOp::List,
                    path: resolved,
                    source,
                })
            }
        }
    }
}

/// Returns the one-letter type an entry is shown with.
fn glyph(kind: &EntryKind) -> char {
    match kind {
        EntryKind::File { .. } => 'f',
        EntryKind::Dir => 'd',
        EntryKind::Symlink { .. } => 'l',
        EntryKind::Other => '?',
    }
}

/// The entries of one directory, up to the cap.
struct Listing {
    entries: Vec<Entry>,
    truncated: bool,
}

/// Says why a directory was not listed. Anything but a directory is the wrong kind.
enum ListDirError {
    Open(OpenError),
    Io(std::io::Error),
}

/// Opens the admitted directory and reads up to `cap` of its entries, sorted by name. It
/// blocks on the filesystem, so it runs on the blocking pool.
fn list(path: &ReadPath, cap: usize) -> Result<Listing, ListDirError> {
    let dir = path.open_dir().map_err(ListDirError::Open)?;
    let (mut entries, truncated) = fs_open::entries(&dir, cap).map_err(ListDirError::Io)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Listing { entries, truncated })
}

fn render_listing(entries: &[Entry], truncated: bool, cap: usize) -> String {
    if entries.is_empty() {
        return String::from("(empty directory)");
    }
    let mut out = String::with_capacity(entries.len() * 32);
    for e in entries {
        out.push(glyph(&e.kind));
        out.push(' ');
        out.push_str(&e.name.to_string_lossy());
        match &e.kind {
            EntryKind::Symlink { target } => {
                out.push_str(" -> ");
                out.push_str(&target.display().to_string());
            }
            EntryKind::File { size } => out.push_str(&format!(" [{size}]")),
            EntryKind::Dir | EntryKind::Other => {}
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
    use serde_json::json;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    use crate::tool::Tool;

    async fn write(path: &Path, content: &[u8]) {
        tokio::fs::write(path, content).await.unwrap();
    }

    #[test]
    fn the_tool_is_named_fs_list_dir() {
        let t = ListDirTool::new();
        assert_eq!(t.name(), "fs.list_dir");
        assert!(t.description().to_lowercase().contains("director"));
    }

    #[test]
    fn schema_marks_path_required_and_no_extra_properties() {
        let t = ListDirTool::new();
        let schema = t.input_schema();
        assert!(schema.strict);
        let required: Vec<&str> = schema.schema()["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["path"]);
        assert_eq!(schema.schema()["additionalProperties"], json!(false));
    }

    #[tokio::test]
    async fn lists_files_and_subdirs_sorted_with_type_glyphs() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("zeta.txt"), b"hi").await;
        write(&dir.path().join("alpha.rs"), b"fn main() {}").await;
        tokio::fs::create_dir(dir.path().join("subdir"))
            .await
            .unwrap();

        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::new());
        let r = tool
            .execute(
                json!({"path": dir.path().to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let lines: Vec<&str> = r.content.lines().collect();
        assert_eq!(lines[0], "f alpha.rs [12]");
        assert_eq!(lines[1], "d subdir");
        assert_eq!(lines[2], "f zeta.txt [2]");
        let basename = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            r.title.as_deref(),
            Some(&*format!("Listed {basename}/ (3 entries)"))
        );
    }

    #[tokio::test]
    async fn empty_directory_renders_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::new());
        let r = tool
            .execute(
                json!({"path": dir.path().to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
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
                &ToolEnv::default(),
                CancellationToken::new(),
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
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("not a directory"));
        assert!(
            r.content.contains("fs.read_file"),
            "should hint at the right tool: {}",
            r.content,
        );
    }

    #[tokio::test]
    async fn truncates_at_max_entries_and_flags_in_output() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            write(&dir.path().join(format!("f{i:02}.txt")), b"x").await;
        }

        let tool: Arc<dyn Tool> =
            Arc::new(ListDirTool::new().max_entries(NonZeroUsize::new(3).unwrap()));
        let r = tool
            .execute(
                json!({"path": dir.path().to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
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
                &ToolEnv::default(),
                CancellationToken::new(),
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
            .execute(
                json!({"not_path": "x"}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .expect_err("should reject malformed args");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn cancelled_before_listing_surfaces_typed_cancelled_error() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolEnv {
            cwd: None,
            ..Default::default()
        };

        let tool: Arc<dyn Tool> = Arc::new(ListDirTool::new());
        let err = tool
            .execute(json!({"path": dir.path().to_str().unwrap()}), &ctx, cancel)
            .await
            .expect_err("pre-cancelled should fast-fail");
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_is_shown_with_its_target_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        let r = ListDirTool::with_root(dir.path())
            .unwrap()
            .execute(
                json!({"path": "."}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            r.content.trim(),
            format!("l out -> {}", outside.path().display())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_became_a_symlink_after_the_check_is_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("d")).unwrap();
        assert!(matches!(
            list(&ReadPath::Canonical(root.join("d")), 10),
            Err(ListDirError::Open(OpenError::Symlink))
        ));
    }
}
