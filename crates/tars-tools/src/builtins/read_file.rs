//! `fs.read_file`: reads a UTF-8 text file, whole or a line window, up to a byte cap. A file over
//! the cap is read whole only as a window: its lines are streamed, those outside the window counted
//! and dropped, and the cap bounds the lines kept.
//!
//! Where it may read is the read jail's decision (see `read_jail`). A confined path is
//! canonicalized before the check, so a symlink out of the roots is refused, and then opened
//! without following a symlink at any component, so one put there after the check is refused
//! too. Only a regular file is read; a FIFO would block the read forever. A missing, binary,
//! oversized or unreadable file is an `is_error` result the model can recover from, not a
//! [`ToolError`].

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};
use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use serde::Deserialize;

use tars_types::JsonSchema;

use super::model_can_fix;
use super::read_jail::{OpenError, Reach, ReadJail, ReadPath};
use crate::tool::{FsOp, Tool, ToolEnv, ToolError, ToolOutput};
use tars_sandbox::{ResolveRootError, canonical_root};

/// Caps the bytes one call reads.
pub const DEFAULT_MAX_BYTES: NonZeroU64 = NonZeroU64::new(256 * 1024).expect("256 KiB is not zero");

/// Holds the description from `read_file.txt`, embedded at compile time so that no file on the
/// host can change a prompt at run time.
static DESCRIPTION_TRIMMED: LazyLock<String> =
    LazyLock::new(|| include_str!("read_file.txt").trim_end().to_string());

pub struct ReadFileTool {
    /// Holds the directory a canonicalized path must sit inside, when set.
    root: Option<PathBuf>,
    max_bytes: NonZeroU64,
}

/// Lines `start..=end` of a file of `total` lines, 1-based and inclusive. `1 <= start <= end <=
/// total` holds because only [`line_range`] makes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineWindow {
    start: usize,
    end: usize,
    total: usize,
}

/// Says why a requested window names no lines of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineRangeError {
    ZeroStart,
    EndBeforeStart { start: u32, end: u32 },
    PastEnd { start: u32, total: usize },
}

impl std::fmt::Display for LineRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroStart => f.write_str("lines are 1-based; `start` 0 names no line"),
            Self::EndBeforeStart { start, end } => {
                write!(f, "`end` {end} precedes `start` {start}")
            }
            Self::PastEnd { start, total } => {
                write!(
                    f,
                    "`start` {start} is past the last line; the file has {total}"
                )
            }
        }
    }
}

/// Returns the lines a window asks for, `end` cut to the last line; `None` asks for the whole
/// file.
fn line_range(
    start: Option<u32>,
    end: Option<u32>,
    total: usize,
) -> Result<Option<LineWindow>, LineRangeError> {
    if start.is_none() && end.is_none() {
        return Ok(None);
    }
    let start = start.unwrap_or(1);
    if start == 0 {
        return Err(LineRangeError::ZeroStart);
    }
    if let Some(end) = end
        && end < start
    {
        return Err(LineRangeError::EndBeforeStart { start, end });
    }
    if start as usize > total {
        return Err(LineRangeError::PastEnd { start, total });
    }
    Ok(Some(LineWindow {
        start: start as usize,
        end: end.map_or(total, |e| (e as usize).min(total)),
        total,
    }))
}

/// Returns the window's text and a note naming which lines of how many they are, since a window
/// that does not say so reads as the whole file.
fn window(
    content: &str,
    start: Option<u32>,
    end: Option<u32>,
) -> Result<(String, String), LineRangeError> {
    let lines: Vec<&str> = content.lines().collect();
    let Some(LineWindow { start, end, total }) = line_range(start, end, lines.len())? else {
        return Ok((content.to_string(), String::new()));
    };
    Ok(window_text(&lines[start - 1..end], start, end, total))
}

/// Returns the text of lines `start..=end` of `total` and the note that names them.
fn window_text(lines: &[&str], start: usize, end: usize, total: usize) -> (String, String) {
    (
        lines.join("\n") + "\n",
        format!(" lines {start}-{end} of {total}"),
    )
}

/// Returns whether 1-based line `n` falls in a window from `first` to `last`, `None` meaning to
/// the end.
fn in_window(n: usize, first: usize, last: Option<usize>) -> bool {
    n >= first && last.is_none_or(|last| n <= last)
}

/// Reads `file` as a stream for the lines `start..=end` asks for, keeping only those and counting
/// the rest, so a file of any size costs at most `cap` bytes of kept lines. Lines end at `\n`,
/// and a `\r` before it is not part of the line, as for [`str::lines`].
fn stream_window(
    file: impl std::io::Read,
    start: Option<u32>,
    end: Option<u32>,
    cap: u64,
) -> Result<(String, String), ReadFileError> {
    use std::io::BufRead;

    // The range is checked first, as far as it can be without the file's length, so a range
    // that names no line is refused before the file is read.
    line_range(start, end, usize::MAX).map_err(ReadFileError::Range)?;
    let first = start.map_or(1, |s| s as usize);
    let last = end.map(|e| e as usize);

    let mut reader = std::io::BufReader::new(file);
    let mut kept: Vec<Vec<u8>> = Vec::new();
    let mut kept_bytes = 0u64;
    let mut line = 1usize;
    let mut partial = false;
    loop {
        let buf = reader.fill_buf().map_err(ReadFileError::Io)?;
        if buf.is_empty() {
            break;
        }
        let (chunk, ends_line) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..i], true),
            None => (buf, false),
        };
        if in_window(line, first, last) {
            if !partial {
                kept.push(Vec::new());
            }
            kept_bytes += (chunk.len() + usize::from(ends_line)) as u64;
            if kept_bytes > cap {
                return Err(ReadFileError::WindowTooLarge { cap });
            }
            if let Some(current) = kept.last_mut() {
                current.extend_from_slice(chunk);
                if ends_line && current.last() == Some(&b'\r') {
                    current.pop();
                }
            }
        }
        let used = chunk.len() + usize::from(ends_line);
        reader.consume(used);
        if ends_line {
            line += 1;
            partial = false;
        } else {
            partial = true;
        }
    }
    // A last line with no `\n` after it is a line; an empty remainder is not.
    let total = if partial { line } else { line - 1 };
    let text: Vec<String> = kept
        .into_iter()
        .map(|bytes| String::from_utf8(bytes).map_err(|_| ReadFileError::NotUtf8))
        .collect::<Result<_, _>>()?;
    let lines: Vec<&str> = text.iter().map(String::as_str).collect();
    match line_range(start, end, total).map_err(ReadFileError::Range)? {
        Some(LineWindow { start, end, total }) => Ok(window_text(&lines, start, end, total)),
        None => Ok((lines.join("\n") + "\n", String::new())),
    }
}

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    start: Option<u32>,
    #[serde(default)]
    end: Option<u32>,
}

impl ReadFileTool {
    /// Returns a tool with no root of its own; it reads anywhere the process can unless
    /// [`ToolEnv::sandbox`] confines it to `cwd` and the readable roots.
    pub fn new() -> Self {
        Self {
            root: None,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Confines reads to `root` and the call's readable roots, refusing a path outside them with
    /// an `is_error` result. `root` is canonicalized now, so a symlink to it behaves as the root.
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

    /// Sets the byte cap; the default is 256 KiB.
    pub fn max_bytes(mut self, n: NonZeroU64) -> Self {
        self.max_bytes = n;
        self
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
        DESCRIPTION_TRIMMED.as_str()
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "ReadFileArgs",
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path. Absolute or relative to the working directory."
                        },
                        "start": {
                            "type": "integer",
                            "description": "First line to return, 1-based and inclusive. Omit for the whole file. Use with `grep`, which reports line numbers: read a window around the hit rather than the whole file."
                        },
                        "end": {
                            "type": "integer",
                            "description": "Last line to return, 1-based and inclusive. Omit to read to the end."
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
        let parsed: ReadFileArgs =
            serde_json::from_value(args).map_err(ToolError::InvalidArguments)?;

        let admitted =
            ReadJail::of(self.root.as_deref(), env, Reach::Path).admit(Path::new(&parsed.path));
        let read_path = match admitted {
            Ok(p) => p,
            Err(refusal) => return Ok(ToolOutput::error(refusal.to_string())),
        };
        let resolved = read_path.path().to_path_buf();
        let basename = resolved
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| resolved.display().to_string());

        let cap = self.max_bytes.get();
        let (start, end) = (parsed.start, parsed.end);
        let read = tokio::task::spawn_blocking(move || read_lines(&read_path, start, end, cap));
        let bytes = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            r = read => match r {
                Ok(r) => r,
                Err(source) => return Err(ToolError::TaskLost {
                tool: "fs.read_file",
                source,
            }),
            },
        };

        match bytes {
            Ok((body, note)) => {
                let title = format!("Read {basename} ({} bytes){note}", body.len());
                Ok(ToolOutput::titled_success(title, body))
            }
            Err(ReadFileError::Range(bad)) => Ok(ToolOutput::titled_error(
                format!("{basename}: bad line range"),
                bad.to_string(),
            )),
            Err(ReadFileError::TooLarge { size }) => Ok(ToolOutput::titled_error(
                format!("{basename} too large ({size} bytes)"),
                format!(
                    "file `{}` is {size} bytes, exceeds the {} byte cap; \
                     read a window of it with `start` and `end`",
                    resolved.display(),
                    self.max_bytes,
                ),
            )),
            Err(ReadFileError::WindowTooLarge { cap }) => Ok(ToolOutput::titled_error(
                format!("{basename}: window too large"),
                format!(
                    "the lines asked for from `{}` exceed the {cap} byte cap; ask for fewer lines",
                    resolved.display(),
                ),
            )),
            Err(ReadFileError::NotUtf8) => Ok(ToolOutput::titled_error(
                format!("{basename} is not UTF-8"),
                format!(
                    "file `{}` is not valid UTF-8 (binary?); fs.read_file only \
                     returns text",
                    resolved.display(),
                ),
            )),
            Err(ReadFileError::Open(OpenError::NotFound)) => Ok(ToolOutput::titled_error(
                format!("{basename} not found"),
                format!("file `{}` not found", resolved.display()),
            )),
            Err(ReadFileError::Open(OpenError::WrongKind)) => Ok(ToolOutput::titled_error(
                format!("{basename} is not a regular file"),
                format!(
                    "`{}` is not a regular file; fs.read_file reads files, fs.list_dir lists \
                     directories",
                    resolved.display(),
                ),
            )),
            Err(ReadFileError::Open(OpenError::Symlink)) => Ok(ToolOutput::titled_error(
                format!("{basename} is a symbolic link"),
                format!(
                    "`{}`, or a directory on the way to it, became a symbolic link after it was \
                     checked; a confined read does not follow it",
                    resolved.display(),
                ),
            )),
            Err(ReadFileError::Open(OpenError::Io(e)) | ReadFileError::Io(e))
                if model_can_fix(e.kind()) =>
            {
                Ok(ToolOutput::titled_error(
                    format!("{basename} cannot be read"),
                    format!("reading `{}`: {e}", resolved.display()),
                ))
            }
            Err(ReadFileError::Open(OpenError::Io(source)) | ReadFileError::Io(source)) => {
                Err(ToolError::Io {
                    op: FsOp::Read,
                    path: resolved,
                    source,
                })
            }
        }
    }
}

/// Says why a file was not read. A directory, FIFO, socket or device is the wrong kind: only a
/// regular file is read.
enum ReadFileError {
    Open(OpenError),
    /// The file is over the cap and no window was asked for.
    TooLarge {
        size: u64,
    },
    /// The lines a window over a file past the cap asks for are themselves over the cap.
    WindowTooLarge {
        cap: u64,
    },
    Range(LineRangeError),
    NotUtf8,
    Io(std::io::Error),
}

/// Opens `path`, a regular file, and returns the text `start..=end` asks for with its note: the
/// whole file when it is at most `cap` bytes, else, when a window was asked for, the window read
/// as a stream. The open does not block on a FIFO; it blocks on the filesystem, so it runs on the
/// blocking pool.
fn read_lines(
    path: &ReadPath,
    start: Option<u32>,
    end: Option<u32>,
    cap: u64,
) -> Result<(String, String), ReadFileError> {
    let file = path.open().map_err(ReadFileError::Open)?;
    // The size and kind are checked on the opened file, before anything is read or allocated.
    let meta = file.metadata().map_err(ReadFileError::Io)?;
    if !meta.is_file() {
        return Err(ReadFileError::Open(OpenError::WrongKind));
    }
    if meta.len() <= cap {
        let content = read_capped(file, meta.len(), cap)?;
        return window(&content, start, end).map_err(ReadFileError::Range);
    }
    if start.is_none() && end.is_none() {
        return Err(ReadFileError::TooLarge { size: meta.len() });
    }
    stream_window(file, start, end, cap)
}

/// Reads `file`, of `len` bytes at its open, whole, up to `cap` bytes.
fn read_capped(file: std::fs::File, len: u64, cap: u64) -> Result<String, ReadFileError> {
    use std::io::Read;

    let mut buf = Vec::with_capacity(len as usize);
    // `take(cap)` bounds a file that grew after the size check.
    file.take(cap)
        .read_to_end(&mut buf)
        .map_err(ReadFileError::Io)?;
    String::from_utf8(buf).map_err(|_| ReadFileError::NotUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    use crate::SandboxPolicy;
    use crate::tool::Tool;

    async fn write(path: &Path, content: &[u8]) {
        tokio::fs::write(path, content).await.unwrap();
    }

    #[test]
    fn the_tool_is_named_fs_read_file() {
        let t = ReadFileTool::new();
        assert_eq!(t.name(), "fs.read_file");
        assert!(t.description().to_lowercase().contains("read"));
    }

    #[test]
    fn schema_marks_path_required_and_no_extra_properties() {
        let t = ReadFileTool::new();
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
    async fn read_file_happy_path_returns_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        write(&path, b"hello world").await;

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
        let r = tool
            .execute(
                json!({"path": path.to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content, "hello world");
        assert_eq!(r.title.as_deref(), Some("Read hello.txt (11 bytes)"));
    }

    #[tokio::test]
    async fn missing_file_yields_is_error_not_hard_error() {
        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
        let r = tool
            .execute(
                json!({"path": "/nonexistent/path/to/nothing.txt"}),
                &ToolEnv::default(),
                CancellationToken::new(),
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
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(
            r.content.contains("UTF-8"),
            "should mention UTF-8: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn over_cap_file_rejects_with_size_in_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        write(&path, &vec![b'a'; 1024]).await;

        let tool: Arc<dyn Tool> =
            Arc::new(ReadFileTool::new().max_bytes(NonZeroU64::new(100).unwrap()));
        let r = tool
            .execute(
                json!({"path": path.to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(
            r.content.contains("1024"),
            "should report file size: {}",
            r.content
        );
        assert!(
            r.content.contains("100"),
            "should report cap: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn jail_blocks_path_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("ok.txt");
        write(&inside, b"inside").await;

        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("naughty.txt");
        write(&outside, b"escaped").await;

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::with_root(dir.path()).unwrap());

        let r = tool
            .execute(
                json!({"path": inside.to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content, "inside");

        let r = tool
            .execute(
                json!({"path": outside.to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
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
        let ctx = ToolEnv {
            cwd: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let r = tool
            .execute(json!({"path": "rel.txt"}), &ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content, "rel ok");
    }

    #[tokio::test]
    async fn sandbox_confines_reads_to_readable_roots() {
        let allowed = tempfile::tempdir().unwrap();
        let inside = allowed.path().join("ok.txt");
        write(&inside, b"inside").await;

        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("secret.txt");
        write(&outside, b"secret").await;

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
        let ctx = ToolEnv {
            sandbox: SandboxPolicy::read_only(false),
            readable_roots: vec![allowed.path().to_path_buf()],
            ..Default::default()
        };

        let r = tool
            .execute(
                json!({"path": inside.to_str().unwrap()}),
                &ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !r.is_error,
            "read inside readable_root must succeed: {}",
            r.content
        );
        assert_eq!(r.content, "inside");

        let r = tool
            .execute(
                json!({"path": outside.to_str().unwrap()}),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "read outside readable_roots must be rejected");
        assert!(
            r.content.contains("outside the allowed root"),
            "got: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn default_sandbox_does_not_restrict_reads() {
        let bystander = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("free.txt");
        write(&path, b"free").await;

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
        let ctx = ToolEnv {
            readable_roots: vec![bystander.path().to_path_buf()],
            ..Default::default()
        };
        let r = tool
            .execute(
                json!({"path": path.to_str().unwrap()}),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !r.is_error,
            "default policy must not restrict reads: {}",
            r.content
        );
        assert_eq!(r.content, "free");
    }

    #[tokio::test]
    async fn invalid_args_yield_typed_invalid_arguments_error() {
        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
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
    async fn cancelled_before_read_surfaces_typed_cancelled_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.txt");
        write(&path, b"hi").await;

        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolEnv {
            cwd: None,
            ..Default::default()
        };

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::new());
        let err = tool
            .execute(json!({"path": path.to_str().unwrap()}), &ctx, cancel)
            .await
            .expect_err("pre-cancelled should fast-fail");
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn a_relative_path_resolves_inside_the_jail_not_the_process_cwd() {
        let jail = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(jail.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "// the worktree copy\n").unwrap();

        let tool: Arc<dyn Tool> = Arc::new(ReadFileTool::with_root(&root).unwrap());
        let r = tool
            .execute(
                json!({ "path": "src/lib.rs" }),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            !r.is_error,
            "relative path inside the jail must resolve: {}",
            r.content
        );
        assert!(r.content.contains("the worktree copy"), "{}", r.content);
    }

    #[test]
    fn no_window_asks_for_the_whole_file() {
        assert_eq!(line_range(None, None, 500), Ok(None));
    }

    #[test]
    fn a_window_ending_past_the_last_line_is_cut_to_it() {
        assert_eq!(
            line_range(Some(499), Some(510), 500),
            Ok(Some(LineWindow {
                start: 499,
                end: 500,
                total: 500
            }))
        );
    }

    #[test]
    fn a_window_starting_on_the_last_line_is_one_line() {
        assert_eq!(
            line_range(Some(500), None, 500),
            Ok(Some(LineWindow {
                start: 500,
                end: 500,
                total: 500
            }))
        );
    }

    #[test]
    fn a_window_starting_past_the_last_line_says_how_many_there_are() {
        assert_eq!(
            line_range(Some(501), Some(510), 500),
            Err(LineRangeError::PastEnd {
                start: 501,
                total: 500
            })
        );
        assert_eq!(
            line_range(Some(900), Some(910), 500),
            Err(LineRangeError::PastEnd {
                start: 900,
                total: 500
            })
        );
    }

    #[test]
    fn an_end_before_the_start_is_named_as_such() {
        assert_eq!(
            line_range(Some(10), Some(9), 500),
            Err(LineRangeError::EndBeforeStart { start: 10, end: 9 })
        );
        assert_eq!(
            line_range(Some(10), Some(10), 500),
            Ok(Some(LineWindow {
                start: 10,
                end: 10,
                total: 500
            }))
        );
    }

    #[test]
    fn a_zero_start_names_no_line() {
        assert_eq!(
            line_range(Some(0), None, 500),
            Err(LineRangeError::ZeroStart)
        );
        assert_eq!(
            line_range(Some(1), None, 500),
            Ok(Some(LineWindow {
                start: 1,
                end: 500,
                total: 500
            }))
        );
    }

    fn confining(cwd: Option<PathBuf>, readable_roots: Vec<PathBuf>) -> ToolEnv {
        ToolEnv {
            cwd,
            sandbox: crate::SandboxPolicy::read_only(false),
            readable_roots,
        }
    }

    #[tokio::test]
    async fn a_confining_sandbox_with_no_read_root_refuses_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"secret").await;
        let r = ReadFileTool::new()
            .execute(
                json!({"path": path.to_str().unwrap()}),
                &confining(None, vec![]),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert!(
            r.content.contains("no cwd and no readable roots"),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn a_read_root_that_does_not_resolve_refuses_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"secret").await;
        let r = ReadFileTool::new()
            .execute(
                json!({"path": path.to_str().unwrap()}),
                &confining(None, vec![dir.path().join("gone")]),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("cannot be resolved"), "{}", r.content);
    }

    #[tokio::test]
    async fn a_stale_readable_root_does_not_refuse_a_read_inside_cwd() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("a.rs"), b"x\n").await;
        let ctx = ToolEnv {
            cwd: Some(root.path().to_path_buf()),
            sandbox: crate::SandboxPolicy::workspace_write(root.path()),
            readable_roots: vec![root.path().join("deps-not-fetched-yet")],
        };
        let r = ReadFileTool::new()
            .execute(json!({ "path": "a.rs" }), &ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
    }

    #[tokio::test]
    async fn a_tool_with_a_root_also_reads_the_readable_roots() {
        let root = tempfile::tempdir().unwrap();
        let dep = tempfile::tempdir().unwrap();
        write(&dep.path().join("lib.rs"), b"dep\n").await;
        let ctx = ToolEnv {
            cwd: Some(root.path().to_path_buf()),
            readable_roots: vec![dep.path().to_path_buf()],
            ..Default::default()
        };
        let path = dep.path().join("lib.rs");
        let r = ReadFileTool::with_root(root.path())
            .unwrap()
            .execute(
                json!({ "path": path.to_str().unwrap() }),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(r.content, "dep\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let tool = ReadFileTool::new();
        let env = ToolEnv::default();
        let read = tool.execute(
            json!({ "path": fifo.to_str().unwrap() }),
            &env,
            CancellationToken::new(),
        );
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), read)
            .await
            .expect("the open does not block on a FIFO")
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("not a regular file"), "{}", r.content);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_socket_is_not_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let _l = std::os::unix::net::UnixListener::bind(root.join("s.sock")).unwrap();
        for sandbox in [
            SandboxPolicy::default(),
            SandboxPolicy::workspace_write(&root),
        ] {
            let ctx = ToolEnv {
                cwd: Some(root.clone()),
                sandbox,
                ..Default::default()
            };
            let r = ReadFileTool::new()
                .execute(json!({ "path": "s.sock" }), &ctx, CancellationToken::new())
                .await
                .unwrap();
            assert!(r.is_error, "{}", r.content);
            assert!(r.content.contains("not a regular file"), "{}", r.content);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_root_directory_is_not_a_regular_file() {
        let ctx = ToolEnv {
            cwd: Some(PathBuf::from("/")),
            sandbox: SandboxPolicy::read_only(true),
            readable_roots: vec![PathBuf::from("/")],
        };
        let r = ReadFileTool::new()
            .execute(json!({ "path": "/" }), &ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("not a regular file"), "{}", r.content);
    }

    #[tokio::test]
    async fn a_window_of_a_file_over_the_cap_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.rs");
        let body: String = (0..40_000).map(|i| format!("line {i}\n")).collect();
        assert!(body.len() as u64 > DEFAULT_MAX_BYTES.get());
        write(&path, body.as_bytes()).await;
        let r = ReadFileTool::new()
            .execute(
                json!({"path": path.to_str().unwrap(), "start": 2, "end": 3}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(r.content, "line 1\nline 2\n");
        assert!(
            r.title.as_deref().unwrap().ends_with("lines 2-3 of 40000"),
            "{:?}",
            r.title
        );
    }

    #[tokio::test]
    async fn a_window_over_the_cap_is_refused_as_too_many_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.rs");
        let body: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        write(&path, body.as_bytes()).await;
        let r = ReadFileTool::new()
            .max_bytes(NonZeroU64::new(100).unwrap())
            .execute(
                json!({"path": path.to_str().unwrap(), "start": 1, "end": 500}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(r.content.contains("ask for fewer lines"), "{}", r.content);
    }

    #[tokio::test]
    async fn a_file_over_the_cap_without_a_window_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.rs");
        write(&path, &[b'a'; 200]).await;
        let r = ReadFileTool::new()
            .max_bytes(NonZeroU64::new(100).unwrap())
            .execute(
                json!({"path": path.to_str().unwrap()}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(r.is_error);
        assert!(
            r.content.contains("exceeds the 100 byte cap"),
            "{}",
            r.content
        );
    }

    #[test]
    fn a_streamed_window_reads_the_lines_a_whole_read_would() {
        let text = "one\r\ntwo\n\nfour\nfive";
        for (start, end) in [
            (Some(1), Some(2)),
            (Some(2), None),
            (None, Some(3)),
            (Some(5), Some(9)),
            (Some(3), Some(3)),
        ] {
            let streamed = stream_window(text.as_bytes(), start, end, 1 << 20).ok();
            assert_eq!(
                streamed,
                window(text, start, end).ok(),
                "{start:?}..{end:?}"
            );
        }
    }

    #[test]
    fn a_streamed_window_past_the_last_line_says_how_many_there_are() {
        let got = stream_window("a\nb\n".as_bytes(), Some(3), None, 1 << 20);
        assert!(matches!(
            got,
            Err(ReadFileError::Range(LineRangeError::PastEnd {
                start: 3,
                total: 2
            }))
        ));
    }
}
