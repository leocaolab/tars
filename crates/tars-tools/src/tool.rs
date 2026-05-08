//! [`Tool`] trait + supporting types.

use std::path::PathBuf;

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use tars_types::JsonSchema;

/// Per-call environment a [`Tool`] receives. Deliberately small today
/// — each field has a concrete consumer right now. Doc 05 §3.3 lists
/// more (principal, tenant, deadline, budget); they slot in as their
/// backing crates ship. Same shape rationale as
/// `tars_runtime::AgentContext`.
#[derive(Clone, Debug, Default)]
pub struct ToolContext {
    /// Cooperative cancellation. Tools that do anything expensive
    /// (file I/O, network, subprocess) should `select!` against
    /// `cancel.cancelled()` so an upstream Drop / SIGINT propagates.
    pub cancel: CancellationToken,
    /// Working directory hint. Tools that touch the filesystem
    /// (`fs.read_file`, future `git.*`) MAY use this to resolve
    /// relative paths. `None` falls back to whatever the tool
    /// considers the default (usually `std::env::current_dir`).
    pub cwd: Option<PathBuf>,
}

/// What a [`Tool::execute`] returns on success. The `content` is what
/// the next LLM turn sees inside [`tars_types::Message::Tool::content`];
/// `is_error=true` flips the same flag on the assembled message so
/// the model knows the tool failed (vs. returned an empty result).
///
/// `title` is a short human-readable summary of what the tool did
/// (`"Read foo.rs"`, `"Listed src/ (23 entries)"`, `"hello.txt not
/// found"`). It is **not** sent to the LLM — that's `content`'s job.
/// Title is for trajectory-log readability + future TUI consumers
/// who need a one-line "what just happened" without parsing the
/// content blob. Empty string means "no title; consumers should fall
/// back to the content head".
///
/// We model failure two ways:
/// - [`ToolError`] — execution couldn't be attempted (bad args, tool
///   not found, cancelled). Surfaces as `Err(_)`; the registry's
///   dispatch helper still produces an `is_error=true` message so
///   the LLM gets a clean signal.
/// - `Ok(ToolResult { is_error: true, content: "explanation" })` —
///   execution ran but produced a logical failure (file not found,
///   git command exited non-zero, HTTP 4xx). The tool itself decides
///   whether to surface this as `Err` or as an `is_error` Ok depending
///   on whether the LLM is expected to recover.
#[derive(Clone, Debug)]
pub struct ToolResult {
    pub title: String,
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    /// Build a successful (`is_error=false`) result with no title.
    /// Prefer [`Self::titled_success`] when the tool can produce a
    /// useful one-line summary.
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            title: String::new(),
            content: content.into(),
            is_error: false,
        }
    }

    /// Build a successful result with a short human-readable title
    /// (`"Read foo.rs"`, etc.). The title is for trajectory log /
    /// TUI readability; it is NOT sent to the LLM.
    pub fn titled_success(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            content: content.into(),
            is_error: false,
        }
    }

    /// Build a logical-failure (`is_error=true`) result with no title.
    /// Use when the tool ran successfully but the *operation* didn't
    /// (the LLM should adapt — e.g. file not found, retry with a
    /// different path). Prefer [`Self::titled_error`] when a one-line
    /// summary is available.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            title: String::new(),
            content: content.into(),
            is_error: true,
        }
    }

    /// Build a logical-failure result with a short human-readable
    /// title (`"hello.txt not found"`, etc.).
    pub fn titled_error(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            content: content.into(),
            is_error: true,
        }
    }
}

/// Errors a Tool can surface to the dispatcher. Distinct from
/// `ToolResult { is_error: true }`: a [`ToolError`] means the tool
/// couldn't even attempt its work; an `is_error` result means it ran
/// and the *operation* failed (the LLM should adapt).
#[derive(Debug, Error)]
pub enum ToolError {
    /// Caller cancelled mid-execution.
    #[error("cancelled")]
    Cancelled,
    /// Provided arguments don't fit the tool's input schema. Carry a
    /// human-readable reason; the dispatcher renders it back to the
    /// LLM so the next turn can retry with corrected args.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// Tool-internal failure (filesystem permission, subprocess
    /// spawn, network unreachable, etc.). Distinct from `Cancelled`
    /// + `InvalidArguments` so callers can class-by-error.
    #[error("execute: {0}")]
    Execute(String),
}

impl ToolError {
    /// One-word classification for logs / metrics. Mirrors the
    /// pattern `tars_runtime::AgentError::classification` uses.
    pub fn classification(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::InvalidArguments(_) => "invalid_arguments",
            Self::Execute(_) => "execute",
        }
    }
}

/// The tool contract. Implementations are stateless wrt one
/// `execute` call: any per-invocation state belongs in the args, any
/// per-tool config belongs on the impl struct. `Arc<dyn Tool>` is the
/// canonical handle (registry + caller share one instance).
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// Stable name. The LLM emits this in `ToolCall.name`; the
    /// registry uses it for lookup. Convention: snake_case with a
    /// `category.action` shape (`fs.read_file`, `git.fetch_pr_diff`,
    /// `web.fetch`) so models grouping tools by category get an
    /// implicit hint.
    fn name(&self) -> &str;

    /// What the tool does — the model uses this to decide *when* to
    /// call it. Doc 05 §3.3: explain when to use, not just what.
    /// Aim for one or two sentences.
    fn description(&self) -> &str;

    /// JSON schema for the input arguments object. The LLM sees this
    /// when picking arguments; the dispatcher does NOT validate
    /// against it today (cost vs. value at first cut — providers'
    /// `strict` mode usually handles it). Tools that care about
    /// invalid args should validate inside `execute` and return
    /// [`ToolError::InvalidArguments`].
    fn input_schema(&self) -> &JsonSchema;

    /// Execute. `args` is always a parsed JSON object (the
    /// `ToolCall::new` constructor enforces this invariant).
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError>;
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
            // Static so we can return a borrow.
            use std::sync::OnceLock;
            static S: OnceLock<JsonSchema> = OnceLock::new();
            S.get_or_init(|| JsonSchema::strict("StubArgs", json!({"type": "object"})))
        }
        async fn execute(
            &self,
            args: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(format!("got: {args}")))
        }
    }

    #[tokio::test]
    async fn tool_trait_round_trip_works() {
        let t: Arc<dyn Tool> = Arc::new(StubTool);
        assert_eq!(t.name(), "stub");
        assert!(t.description().contains("test tool"));
        let r = t
            .execute(json!({"k": "v"}), ToolContext::default())
            .await
            .unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("\"k\""));
    }

    #[test]
    fn tool_result_constructors() {
        let s = ToolResult::success("ok");
        assert!(!s.is_error);
        assert!(
            s.title.is_empty(),
            "untitled success defaults to empty title"
        );
        let e = ToolResult::error("nope");
        assert!(e.is_error);
        assert_eq!(e.content, "nope");
        assert!(e.title.is_empty());
    }

    #[test]
    fn titled_constructors_carry_the_title_string() {
        let s = ToolResult::titled_success("Read foo.rs", "fn main() {}");
        assert_eq!(s.title, "Read foo.rs");
        assert_eq!(s.content, "fn main() {}");
        assert!(!s.is_error);
        let e = ToolResult::titled_error("foo.rs not found", "no such file");
        assert_eq!(e.title, "foo.rs not found");
        assert!(e.is_error);
    }

    #[test]
    fn tool_error_classification_is_stable() {
        assert_eq!(ToolError::Cancelled.classification(), "cancelled");
        assert_eq!(
            ToolError::InvalidArguments("x".into()).classification(),
            "invalid_arguments"
        );
        assert_eq!(ToolError::Execute("x".into()).classification(), "execute");
    }
}
