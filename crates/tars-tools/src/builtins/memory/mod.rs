//! `memory` — the Claude API memory tool, copied: six commands (`view`, `create`,
//! `str_replace`, `insert`, `delete`, `rename`) over a directory the model addresses
//! as `/memories`. Command names, parameters and returned text follow
//! <https://platform.claude.com/docs/en/agents-and-tools/tool-use/memory-tool>.
//!
//! On top of the API tool, two Claude Code behaviours
//! (<https://code.claude.com/docs/en/memory>): every write stamps `modified` and
//! `origin_session_id` into the file's frontmatter, and a write that leaves
//! `MEMORY.md` past what the next session loads (200 lines / 25KB) succeeds but
//! comes back as an error telling the model to rewrite the index.
//!
//! The directory is injected ([`MemoryRoot`], validated once); nothing here reads
//! the environment.

mod error;
mod path;
mod text;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use tars_types::step::context::Clock;
use tars_types::{JsonSchema, SessionId, TenantId};
use tars_utils::display_chain;
use thiserror::Error;

use super::model_can_fix;
use crate::tool::{Tool, ToolEnv, ToolError, ToolOutput};

use error::{IoOp, MemoryError};
pub use path::{MemoryDirError, MemoryRoot, MemoryScope, ProjectKey, memory_dir};
use path::{VirtualPath, resolve};
use text::{
    INDEX_FILE, IndexOverflow, ListingEntry, Stamp, ViewRange, loaded_index, truncation_warning,
};

/// The memory rules, shipped with the memory: Claude Code's `# Memory` system-prompt
/// section, copied from the Claude Code 2.1.284 binary (the builder for a private
/// directory with no team store and no citations). Edits: the directory path reads
/// `/memories`; the sentence naming Claude Code's Write tool is gone; `CLAUDE.md` reads
/// "your instructions" and `<system-reminder>` blocks read "the `MEMORY.md` section
/// below", the places tars puts those two things.
pub const MEMORY_RULES: &str = include_str!("rules.md");

/// A session's memory as it stood when the session started: the directory the `memory`
/// tool works in, and the part of `MEMORY.md` that was loaded. Loaded once, like Claude
/// Code: a write during the session changes the files, not this session's prompt.
#[derive(Clone, Debug)]
pub struct SessionMemory {
    root: MemoryRoot,
    index: Option<String>,
}

/// A session's memory could not be opened.
#[derive(Debug, Error)]
pub enum SessionMemoryError {
    #[error(transparent)]
    Layout(#[from] MemoryDirError),
    #[error("cannot read memory index {}", path.display())]
    ReadIndex { path: PathBuf, source: io::Error },
}

impl SessionMemory {
    /// Opens the memory of `scope` for `tenant` under `tars_home`. A missing `MEMORY.md` is no
    /// index, not an error: a first session has written nothing yet.
    pub fn open(
        tars_home: &Path,
        tenant: &TenantId,
        scope: &MemoryScope,
    ) -> Result<Self, SessionMemoryError> {
        let root = MemoryRoot::open(memory_dir(tars_home, tenant, scope)?)?;
        let path = root.path().join(INDEX_FILE);
        let index = match std::fs::read_to_string(&path) {
            Ok(text) => loaded_index(&text).map(|loaded| match truncation_warning(&text, loaded) {
                Some(warning) => format!("{loaded}\n\n> WARNING: {warning}"),
                None => loaded.to_string(),
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(SessionMemoryError::ReadIndex { path, source }),
        };
        Ok(Self { root, index })
    }

    pub fn root(&self) -> &MemoryRoot {
        &self.root
    }

    /// Returns the rules, then the loaded index under `## MEMORY.md` when there is one.
    pub fn prompt(&self) -> String {
        match &self.index {
            Some(index) => format!("{}\n\n## {INDEX_FILE}\n\n{index}", MEMORY_RULES.trim_end()),
            None => MEMORY_RULES.trim_end().to_string(),
        }
    }
}

/// API doc: directory listings stop two levels down and skip `node_modules`.
const LISTING_DEPTH: usize = 2;
const SKIPPED_DIR: &str = "node_modules";

pub struct MemoryTool {
    root: MemoryRoot,
    /// The session whose writes this tool stamps as `origin_session_id`.
    session: SessionId,
    clock: Arc<dyn Clock>,
}

impl MemoryTool {
    pub fn new(root: MemoryRoot, session: SessionId) -> Self {
        Self::with_clock(root, session, Arc::new(SystemClock))
    }

    pub fn with_clock(root: MemoryRoot, session: SessionId, clock: Arc<dyn Clock>) -> Self {
        Self {
            root,
            session,
            clock,
        }
    }
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum MemoryCommand {
    View {
        path: String,
        view_range: Option<ViewRange>,
    },
    Create {
        path: String,
        file_text: String,
    },
    StrReplace {
        path: String,
        old_str: String,
        /// The API documents an omitted `new_str` as deleting `old_str`.
        #[serde(default)]
        new_str: String,
    },
    Insert {
        path: String,
        insert_line: i64,
        insert_text: String,
    },
    Delete {
        path: String,
    },
    Rename {
        old_path: String,
        new_path: String,
    },
}

/// A command that ran. `overflow` is set when the write left `MEMORY.md` too long to
/// load in full — the write stands, but the model must hear about it as an error.
struct Reply {
    text: String,
    overflow: Option<IndexOverflow>,
}

impl Reply {
    fn done(text: String) -> Self {
        Self {
            text,
            overflow: None,
        }
    }

    fn into_result(self) -> ToolOutput {
        match self.overflow {
            None => ToolOutput::success(self.text),
            Some(o) => ToolOutput::error(text::index_overflow_message(&self.text, &o)),
        }
    }
}

impl MemoryTool {
    async fn run(&self, command: MemoryCommand) -> Result<Reply, MemoryError> {
        match command {
            MemoryCommand::View { path, view_range } => self.view(&path, view_range).await,
            MemoryCommand::Create { path, file_text } => self.create(&path, &file_text).await,
            MemoryCommand::StrReplace {
                path,
                old_str,
                new_str,
            } => self.str_replace(&path, &old_str, &new_str).await,
            MemoryCommand::Insert {
                path,
                insert_line,
                insert_text,
            } => self.insert(&path, insert_line, &insert_text).await,
            MemoryCommand::Delete { path } => self.delete(&path).await,
            MemoryCommand::Rename { old_path, new_path } => self.rename(&old_path, &new_path).await,
        }
    }

    async fn locate(&self, raw: &str) -> Result<(VirtualPath, PathBuf), MemoryError> {
        let path = VirtualPath::parse(raw)?;
        let disk = resolve(&self.root, &path).await?;
        Ok((path, disk))
    }

    async fn view(&self, raw: &str, view_range: Option<ViewRange>) -> Result<Reply, MemoryError> {
        let (path, disk) = self.locate(raw).await?;
        let shown = path.to_string();
        let meta = match tokio::fs::metadata(&disk).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(MemoryError::ViewMissing(shown));
            }
            Err(source) => return Err(io_error(IoOp::Read, &shown, &disk, source)),
        };
        let text = if meta.is_dir() {
            let entries = list_tree(&disk)
                .await
                .map_err(|source| io_error(IoOp::List, &shown, &disk, source))?;
            text::render_listing(&shown, meta.len(), &entries)
        } else {
            let content = read_text(&disk, &shown).await?;
            text::render_file(&shown, &content, view_range)?
        };
        Ok(Reply::done(text))
    }

    async fn create(&self, raw: &str, file_text: &str) -> Result<Reply, MemoryError> {
        let (path, disk) = self.locate(raw).await?;
        let shown = path.to_string();
        let stamped = text::stamp(file_text, &self.stamp());
        create_new(&disk, &stamped, &shown).await?;
        Ok(self.written(
            &path,
            &stamped,
            format!("File created successfully at: {shown}"),
        ))
    }

    async fn str_replace(
        &self,
        raw: &str,
        old_str: &str,
        new_str: &str,
    ) -> Result<Reply, MemoryError> {
        let (path, disk) = self.locate(raw).await?;
        let shown = path.to_string();
        let current = read_existing_file(&disk, &shown, MemoryError::ReplaceMissing).await?;

        let replaced = text::replace_once(&shown, &current, old_str, new_str)?;
        let stamped = text::stamp(&replaced.text, &self.stamp());
        write_text(&disk, &stamped, &shown).await?;

        let line = text::stamped_line(&replaced.text, &stamped, replaced.line);
        Ok(self.written(&path, &stamped, text::replace_snippet(&stamped, line)))
    }

    async fn insert(
        &self,
        raw: &str,
        insert_line: i64,
        insert_text: &str,
    ) -> Result<Reply, MemoryError> {
        let (path, disk) = self.locate(raw).await?;
        let shown = path.to_string();
        let current = read_existing_file(&disk, &shown, MemoryError::Missing).await?;

        let inserted = text::insert_at(&current, insert_line, insert_text)?;
        let stamped = text::stamp(&inserted, &self.stamp());
        write_text(&disk, &stamped, &shown).await?;

        Ok(self.written(
            &path,
            &stamped,
            format!("The file {shown} has been edited."),
        ))
    }

    async fn delete(&self, raw: &str) -> Result<Reply, MemoryError> {
        let path = VirtualPath::parse(raw)?;
        if path.is_root() {
            return Err(MemoryError::DeleteRoot);
        }
        let disk = resolve(&self.root, &path).await?;
        let shown = path.to_string();
        let removed = match tokio::fs::symlink_metadata(&disk).await {
            Ok(meta) if meta.is_dir() => tokio::fs::remove_dir_all(&disk).await,
            Ok(_) => tokio::fs::remove_file(&disk).await,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(MemoryError::Missing(shown));
            }
            Err(e) => Err(e),
        };
        removed.map_err(|source| io_error(IoOp::Delete, &shown, &disk, source))?;
        Ok(Reply::done(format!("Successfully deleted {shown}")))
    }

    async fn rename(&self, old_raw: &str, new_raw: &str) -> Result<Reply, MemoryError> {
        let old = VirtualPath::parse(old_raw)?;
        if old.is_root() {
            return Err(MemoryError::RenameRoot);
        }
        let old_disk = resolve(&self.root, &old).await?;
        let (new, new_disk) = self.locate(new_raw).await?;
        let (old_shown, new_shown) = (old.to_string(), new.to_string());

        if !exists(&old_disk).await {
            return Err(MemoryError::Missing(old_shown));
        }
        if exists(&new_disk).await {
            return Err(MemoryError::DestinationExists(new_shown));
        }
        ensure_parent(&new_disk, &new_shown).await?;
        tokio::fs::rename(&old_disk, &new_disk)
            .await
            .map_err(|source| io_error(IoOp::Rename, &old_shown, &old_disk, source))?;
        Ok(Reply::done(format!(
            "Successfully renamed {old_shown} to {new_shown}"
        )))
    }

    fn stamp(&self) -> Stamp<'_> {
        Stamp {
            modified: self.clock.now(),
            origin_session_id: &self.session,
        }
    }

    /// Returns a write's reply, measuring `/memories/MEMORY.md` when that is what was written.
    fn written(&self, path: &VirtualPath, content: &str, text: String) -> Reply {
        let overflow = path
            .is_root_file(INDEX_FILE)
            .then(|| text::index_overflow(content))
            .flatten();
        Reply { text, overflow }
    }
}

fn io_error(op: IoOp, path: &str, disk: &Path, source: io::Error) -> MemoryError {
    MemoryError::Io {
        op,
        path: path.to_string(),
        disk: disk.to_path_buf(),
        source,
    }
}

/// Returns what a command came to for the caller: a reply, or a failure the model can act on, is
/// text for the model; a filesystem failure the model cannot get past by choosing another path
/// (a full disk, a broken mount) is the host's, so the agent stops or retries rather than the
/// model reading the same failure every turn.
fn outcome(result: Result<Reply, MemoryError>) -> Result<ToolOutput, ToolError> {
    match result {
        Ok(reply) => Ok(reply.into_result()),
        Err(MemoryError::Io {
            op, disk, source, ..
        }) if !model_can_fix(source.kind()) => Err(ToolError::Io {
            op: op.fs_op(),
            path: disk,
            source,
        }),
        Err(e) => Ok(ToolOutput::error(display_chain(&e))),
    }
}

async fn exists(disk: &Path) -> bool {
    tokio::fs::symlink_metadata(disk).await.is_ok()
}

async fn read_text(disk: &Path, shown: &str) -> Result<String, MemoryError> {
    tokio::fs::read_to_string(disk)
        .await
        .map_err(|source| io_error(IoOp::Read, shown, disk, source))
}

/// Reads the file an edit command targets; a missing path or a directory is the command's own
/// "does not exist" error (API doc: "If the path is a directory, return a 'file does not exist'
/// error").
async fn read_existing_file(
    disk: &Path,
    shown: &str,
    missing: fn(String) -> MemoryError,
) -> Result<String, MemoryError> {
    match tokio::fs::metadata(disk).await {
        Ok(meta) if meta.is_file() => read_text(disk, shown).await,
        Ok(_) => Err(missing(shown.to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(missing(shown.to_string())),
        Err(source) => Err(io_error(IoOp::Read, shown, disk, source)),
    }
}

async fn ensure_parent(disk: &Path, shown: &str) -> Result<(), MemoryError> {
    match disk.parent() {
        Some(parent) => tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| io_error(IoOp::Write, shown, parent, source)),
        None => Ok(()),
    }
}

async fn create_new(disk: &Path, content: &str, shown: &str) -> Result<(), MemoryError> {
    use tokio::io::AsyncWriteExt;

    ensure_parent(disk, shown).await?;
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(disk)
        .await
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(MemoryError::AlreadyExists(shown.to_string()));
        }
        Err(source) => return Err(io_error(IoOp::Write, shown, disk, source)),
    };
    // tokio's `File` hands writes to a blocking thread; only `flush` waits for them to land.
    let written = match file.write_all(content.as_bytes()).await {
        Ok(()) => file.flush().await,
        Err(e) => Err(e),
    };
    written.map_err(|source| io_error(IoOp::Write, shown, disk, source))
}

async fn write_text(disk: &Path, content: &str, shown: &str) -> Result<(), MemoryError> {
    tokio::fs::write(disk, content)
        .await
        .map_err(|source| io_error(IoOp::Write, shown, disk, source))
}

/// Lists `dir` depth-first with each directory's children sorted by name, the SDK's order.
async fn list_tree(dir: &Path) -> io::Result<Vec<ListingEntry>> {
    let mut pending = children(dir, "", 1).await?;
    pending.reverse();
    let mut listed = Vec::new();
    while let Some(child) = pending.pop() {
        if child.is_dir && child.depth < LISTING_DEPTH {
            let mut deeper = children(&child.disk, &child.relative, child.depth + 1).await?;
            deeper.reverse();
            pending.extend(deeper);
        }
        listed.push(ListingEntry {
            size: child.size,
            relative: child.relative,
            is_dir: child.is_dir,
        });
    }
    Ok(listed)
}

struct Child {
    disk: PathBuf,
    relative: String,
    depth: usize,
    is_dir: bool,
    size: u64,
}

async fn children(dir: &Path, relative: &str, depth: usize) -> io::Result<Vec<Child>> {
    let mut read = tokio::fs::read_dir(dir).await?;
    let mut found = Vec::new();
    while let Some(entry) = read.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == SKIPPED_DIR {
            continue;
        }
        let meta = tokio::fs::metadata(entry.path()).await?;
        let relative = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}/{name}")
        };
        found.push((
            name,
            Child {
                disk: entry.path(),
                relative,
                depth,
                is_dir: meta.is_dir(),
                size: meta.len(),
            },
        ));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found.into_iter().map(|(_, child)| child).collect())
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Your persistent memory directory, /memories, kept across sessions. \
         Commands: view (a directory listing, or a file with line numbers; \
         optional view_range [start, end], end -1 = to the end), create (a new file), \
         str_replace (replace a unique old_str with new_str), insert (insert_text after \
         line insert_line; 0 = at the top), delete (a file or directory), rename \
         (old_path to new_path). All paths start with /memories. The /memories \
         directory itself cannot be deleted or renamed."
    }

    fn input_schema(&self) -> &JsonSchema {
        static S: OnceLock<JsonSchema> = OnceLock::new();
        S.get_or_init(|| {
            JsonSchema::strict(
                "MemoryArgs",
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "command": {
                            "type": "string",
                            "enum": ["view", "create", "str_replace", "insert", "delete", "rename"]
                        },
                        "path": { "type": "string", "description": "A path under /memories." },
                        "view_range": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "minItems": 2,
                            "maxItems": 2,
                            "description": "view: [start_line, end_line]; end_line -1 = to the end."
                        },
                        "file_text": { "type": "string", "description": "create: the file's content." },
                        "old_str": { "type": "string", "description": "str_replace: text that appears exactly once." },
                        "new_str": { "type": "string", "description": "str_replace: the replacement; omit to delete old_str." },
                        "insert_line": { "type": "integer", "description": "insert: insert after this line; 0 = at the top." },
                        "insert_text": { "type": "string", "description": "insert: the text to insert." },
                        "old_path": { "type": "string", "description": "rename: the path to move." },
                        "new_path": { "type": "string", "description": "rename: where to move it; must not exist." }
                    },
                    "required": ["command"]
                },
            )
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _env: &ToolEnv,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let command: MemoryCommand =
            serde_json::from_value(args).map_err(ToolError::InvalidArguments)?;
        let result = tokio::select! {
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            result = self.run(command) => result,
        };
        outcome(result)
    }
}

#[cfg(test)]
mod tests;
