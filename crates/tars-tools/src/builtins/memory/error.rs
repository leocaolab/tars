//! Why a memory command did not do what it was asked. Each message is the text the
//! Claude API memory-tool doc prescribes for that case (or the Python SDK's
//! `BetaLocalFilesystemMemoryTool` where the doc gives none), so the model reads
//! exactly what Claude would. An `Io` failure the model cannot get past by choosing
//! another path is not shown to it: the tool returns it to the caller as a `ToolError`.

use std::fmt;
use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::tool::FsOp;

#[derive(Debug, Error)]
pub(super) enum MemoryError {
    #[error("Path must start with /memories, got: {0}")]
    OutsidePrefix(String),
    #[error("Path {0} would escape /memories directory")]
    Traversal(String),

    #[error("The path {0} does not exist. Please provide a valid path.")]
    ViewMissing(String),
    #[error("File {0} exceeds maximum line limit of 999,999 lines.")]
    TooManyLines(String),

    #[error("Error: File {0} already exists")]
    AlreadyExists(String),

    #[error("Error: The path {0} does not exist. Please provide a valid path.")]
    ReplaceMissing(String),
    #[error("No replacement was performed, old_str `{old_str}` did not appear verbatim in {path}.")]
    NoMatch { path: String, old_str: String },
    #[error(
        "No replacement was performed. Multiple occurrences of old_str `{old_str}` in lines: {}. Please ensure it is unique",
        join(lines)
    )]
    Ambiguous { old_str: String, lines: Vec<usize> },

    #[error("Error: The path {0} does not exist")]
    Missing(String),
    #[error(
        "Error: Invalid `insert_line` parameter: {insert_line}. It should be within the range of lines of the file: [0, {n_lines}]"
    )]
    InsertLine { insert_line: i64, n_lines: usize },

    #[error("Cannot delete the /memories directory itself")]
    DeleteRoot,
    #[error("Cannot rename the /memories directory itself")]
    RenameRoot,
    #[error("Error: The destination {0} already exists")]
    DestinationExists(String),

    /// `path` is what the model named, for its message; `disk` is where that is on the host,
    /// for the caller when the failure is the host's.
    #[error("Error: cannot {op} {path}")]
    Io {
        op: IoOp,
        path: String,
        disk: PathBuf,
        source: io::Error,
    },
}

/// The filesystem step that failed, named in the message.
#[derive(Clone, Copy, Debug)]
pub(super) enum IoOp {
    Resolve,
    Read,
    Write,
    List,
    Delete,
    Rename,
}

impl IoOp {
    /// Returns the step as the caller's [`FsOp`]; resolving a path is inspecting it.
    pub(super) fn fs_op(self) -> FsOp {
        match self {
            Self::Resolve => FsOp::Inspect,
            Self::Read => FsOp::Read,
            Self::Write => FsOp::Write,
            Self::List => FsOp::List,
            Self::Delete => FsOp::Delete,
            Self::Rename => FsOp::Rename,
        }
    }
}

impl fmt::Display for IoOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Resolve => "resolve",
            Self::Read => "read",
            Self::Write => "write",
            Self::List => "list",
            Self::Delete => "delete",
            Self::Rename => "rename",
        })
    }
}

fn join(lines: &[usize]) -> String {
    lines
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
