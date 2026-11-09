//! Defines what a tool is, what it is given per call, and how it reports success and failure.

use std::io;
use std::path::PathBuf;

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use tars_sandbox::{FindGitDirError, ResolveRootError, SandboxError};
use tars_types::JsonSchema;

use crate::SandboxPolicy;

/// The environment a [`Tool`] runs in, fixed when the tool is put together and the same for
/// every call. `ToolEnv::default()` confines nothing.
#[derive(Clone, Debug, Default)]
pub struct ToolEnv {
    /// Resolves relative paths for the filesystem tools, and bounds a confined read, list or
    /// search when the tool has no root of its own.
    pub cwd: Option<PathBuf>,
    /// Confines the call: `bash.run` runs under it, the read and list tools then read only under
    /// their root, else `cwd`, the readable roots and the writable roots, and the write tools
    /// refuse paths it does not allow, the protected git directories of the workdir and the
    /// writable roots among them (the workdir is `cwd`, else the tool's root). With no root on
    /// the tool and `DangerFullAccess` here, the read tools read and the write tools write
    /// anywhere the process can; `fs.grep` and `fs.glob` stay confined. `web.fetch` and
    /// `web.search` refuse when it cuts the network.
    pub sandbox: SandboxPolicy,
    /// Lists roots outside the confining root that the search and read tools may also read, such
    /// as a project's dependency sources. Write tools ignore it.
    pub readable_roots: Vec<PathBuf>,
}

/// The output of a tool that ran. A failed operation the model can recover from, such as a
/// missing file or an HTTP 4xx, is `is_error: true` here; [`ToolError`] is for a call that has
/// no output for the model.
#[derive(Clone, Debug)]
pub struct ToolOutput {
    /// A one-line summary for logs (`"Read foo.rs"`); it is not sent to the model.
    pub title: Option<String>,
    /// The text the model sees as the tool message.
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    /// Returns a successful output with no title.
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            title: None,
            content: content.into(),
            is_error: false,
        }
    }

    /// Returns a successful output with a title.
    pub fn titled_success(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            content: content.into(),
            is_error: false,
        }
    }

    /// Returns a failed operation with no title.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            title: None,
            content: content.into(),
            is_error: true,
        }
    }

    /// Returns a failed operation with a title.
    pub fn titled_error(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            content: content.into(),
            is_error: true,
        }
    }
}

/// The filesystem operation that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsOp {
    Open,
    CreateParent,
    Inspect,
    Read,
    Write,
    List,
    Delete,
    Rename,
}

impl std::fmt::Display for FsOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FsOp::Open => "open",
            FsOp::CreateParent => "create parent of",
            FsOp::Inspect => "inspect",
            FsOp::Read => "read",
            FsOp::Write => "write",
            FsOp::List => "list",
            FsOp::Delete => "delete",
            FsOp::Rename => "rename",
        })
    }
}

/// Says why a call has no output for the model: it was not attempted (bad arguments, no sandbox,
/// no such tool), it failed in a way only the host can mend (the filesystem, a lost task), or it
/// was cancelled, which may come after a write already landed. [`ToolRegistry::dispatch`](crate::ToolRegistry::dispatch)
/// still turns it into an `is_error` tool message for the model.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("cancelled")]
    Cancelled,
    /// The arguments do not fit the tool's input; the model sees why and can retry.
    #[error("invalid arguments")]
    InvalidArguments(#[source] serde_json::Error),
    /// The filesystem failed in a way the model cannot route around by choosing another path.
    #[error("{op} `{}`", .path.display())]
    Io {
        op: FsOp,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// `sh` could not be started. `WouldBlock` is how a `fork` refused for want of processes
    /// (`EAGAIN`) arrives; then nothing ran, and another try may pass
    /// ([`ToolError::is_transient`]).
    #[error("spawn `sh -c`")]
    Spawn(#[source] io::Error),
    /// The sandbox the command must run under could not be built, so it did not run.
    #[error("exec sandbox")]
    Sandbox(#[source] SandboxError),
    /// The workdir of a confined write cannot be resolved, so what to protect cannot be told.
    #[error("write sandbox workdir")]
    WriteWorkdir(#[source] ResolveRootError),
    /// A `.git` a confined write may need to protect cannot be read.
    #[error("write sandbox")]
    WriteGitDir(#[source] FindGitDirError),
    /// The blocking task a tool ran its work on panicked, or was cancelled with the runtime, so
    /// the call has no result; the source carries the panic's message.
    #[error("the {tool} task did not return")]
    TaskLost {
        tool: &'static str,
        #[source]
        source: tokio::task::JoinError,
    },
    /// The model called a tool no one registered under that name.
    #[error("no tool registered with name `{name}`")]
    UnknownTool { name: String },
    /// A tool implemented outside Rust failed; `message` is what it reported, the only account
    /// of the failure there is.
    #[error("{message}")]
    Foreign { message: String },
}

impl ToolError {
    /// Returns whether the call had no effect and may pass on another try.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Spawn(e) if e.kind() == io::ErrorKind::WouldBlock)
    }

    /// Returns the kind of failure as one word for logs and metrics.
    pub fn classification(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::InvalidArguments(_) => "invalid_arguments",
            Self::UnknownTool { .. } => "unknown_tool",
            e if e.is_transient() => "execute_transient",
            Self::Io { .. }
            | Self::Spawn(_)
            | Self::Sandbox(_)
            | Self::WriteWorkdir(_)
            | Self::WriteGitDir(_)
            | Self::TaskLost { .. }
            | Self::Foreign { .. } => "execute",
        }
    }
}

/// A tool the model can call. An implementation keeps no state between calls: what varies per
/// call is in the arguments, configuration is on the implementing struct.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// Returns the name the model calls the tool by, shaped `category.action` (`fs.read_file`,
    /// `web.fetch`) so the category groups related tools for the model.
    fn name(&self) -> &str;

    /// Returns the one or two sentences the model reads to decide when to call the tool, so it
    /// says when to use it, not only what it does.
    fn description(&self) -> &str;

    /// Returns the JSON schema the model fills the arguments from. The registry does not validate
    /// arguments against it; `execute` checks them and returns [`ToolError::InvalidArguments`].
    fn input_schema(&self) -> &JsonSchema;

    /// Runs one call in `env`. `args` is a JSON object whenever the call was built by
    /// `ToolCall::new` or deserialized; a tool doing file, network or subprocess work selects on
    /// `cancel`.
    async fn execute(
        &self,
        args: serde_json::Value,
        env: &ToolEnv,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use serde_json::json;

    struct StubTool;

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            "stub"
        }
        fn description(&self) -> &str {
            "no-op test tool"
        }
        fn input_schema(&self) -> &JsonSchema {
            use std::sync::OnceLock;
            static S: OnceLock<JsonSchema> = OnceLock::new();
            S.get_or_init(|| {
                JsonSchema::strict("StubArgs", tars_types::json_object! {"type": "object"})
            })
        }
        async fn execute(
            &self,
            args: serde_json::Value,
            _env: &ToolEnv,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::success(format!("got: {args}")))
        }
    }

    #[tokio::test]
    async fn tool_trait_round_trip_works() {
        let t: Arc<dyn Tool> = Arc::new(StubTool);
        assert_eq!(t.name(), "stub");
        assert!(t.description().contains("test tool"));
        let r = t
            .execute(
                json!({"k": "v"}),
                &ToolEnv::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("\"k\""));
    }

    #[test]
    fn tool_result_constructors() {
        let s = ToolOutput::success("ok");
        assert!(!s.is_error);
        assert!(s.title.is_none(), "untitled success has no title");
        let e = ToolOutput::error("nope");
        assert!(e.is_error);
        assert_eq!(e.content, "nope");
        assert!(e.title.is_none());
    }

    #[test]
    fn titled_constructors_carry_the_title_string() {
        let s = ToolOutput::titled_success("Read foo.rs", "fn main() {}");
        assert_eq!(s.title.as_deref(), Some("Read foo.rs"));
        assert_eq!(s.content, "fn main() {}");
        assert!(!s.is_error);
        let e = ToolOutput::titled_error("foo.rs not found", "no such file");
        assert_eq!(e.title.as_deref(), Some("foo.rs not found"));
        assert!(e.is_error);
    }

    #[test]
    fn tool_error_classification_is_stable() {
        assert_eq!(ToolError::Cancelled.classification(), "cancelled");
        assert_eq!(
            ToolError::InvalidArguments(serde_json::from_str::<u8>("x").unwrap_err())
                .classification(),
            "invalid_arguments"
        );
        assert_eq!(
            ToolError::Foreign {
                message: "x".into()
            }
            .classification(),
            "execute"
        );
        assert_eq!(
            ToolError::UnknownTool { name: "x".into() }.classification(),
            "unknown_tool"
        );
        assert_eq!(
            ToolError::Spawn(io::Error::from(io::ErrorKind::WouldBlock)).classification(),
            "execute_transient"
        );
    }
}
