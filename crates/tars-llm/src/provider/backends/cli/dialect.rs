//! Describes what differs between coding-agent CLIs: the argv, where the prompt goes, how the
//! answer comes out, and how it becomes [`ChatEvent`]s. A dialect does not spawn, jail or time
//! anything; the runner does.

use serde_json::Value;

use crate::ProviderError;
use tars_types::{Caller, ChatEvent, ChatRequest, StopReason, Usage};

use super::invocation::SubprocessInvocation;

/// Where a dialect wants the prompt delivered to the child process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptChannel {
    /// The prompt is written to the child's stdin, which is closed after it.
    Stdin,
    /// The prompt is an argument the dialect puts in the argv; stdin is `/dev/null`.
    Arg,
}

/// How the CLI prints its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputMode {
    /// The CLI prints JSON, read by [`CliDialect::parse_line`].
    JsonEvents,
    /// The CLI prints plain text, read by [`CliDialect::parse_text`].
    Text,
}

/// How [`SharedCliRunner`](super::subprocess::SharedCliRunner) turns stdout into the one
/// [`Value`] the dialect reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFraming {
    /// Stdout is one JSON object; with `strip_prefix`, anything before the first `{` is dropped
    /// first.
    SingleObject { strip_prefix: bool },
    /// Stdout is one JSON value per line, handed on as an array of the lines as strings.
    JsonLinesArray,
    /// Stdout is plain text, handed on as one string.
    RawText,
}

/// What one coding-agent CLI needs that the others do not.
pub trait CliDialect: Send + Sync {
    /// Returns the arguments, without the executable.
    fn argv(&self, inv: &SubprocessInvocation) -> Vec<String>;

    /// Returns the invocation for one request: executable, model, prompt, the environment to
    /// strip, cwd and the CLI's own flags.
    ///
    /// # Errors
    ///
    /// Fails when the CLI cannot serve the request, such as a model it does not name.
    fn invocation(
        &self,
        req: &ChatRequest,
        model: &str,
        ctx: &Caller,
    ) -> Result<SubprocessInvocation, ProviderError>;

    /// Returns where the prompt goes.
    fn prompt_channel(&self) -> PromptChannel;

    /// Returns how the CLI prints its answer.
    fn output_mode(&self) -> OutputMode;

    /// Returns how the shared runner frames stdout; by default one JSON object, nothing
    /// stripped.
    fn output_framing(&self) -> OutputFraming {
        OutputFraming::SingleObject {
            strip_prefix: false,
        }
    }

    /// Returns the events of a `JsonEvents` CLI's output: deltas and the final `Finished`. The
    /// backend adds `Started` and cuts the reply to the output budget.
    ///
    /// # Errors
    ///
    /// Fails on a shape it cannot read, carrying the raw line.
    ///
    /// # Panics
    ///
    /// The default panics; only a `Text` dialect may leave it.
    fn parse_line(&self, _raw: &Value) -> Result<Vec<ChatEvent>, ProviderError> {
        unimplemented!("parse_line is only for OutputMode::JsonEvents dialects")
    }

    /// Returns the events of a `Text` CLI's stdout; by default one delta of all of it and an
    /// `EndTurn` finish.
    fn parse_text(&self, stdout: &str) -> Result<Vec<ChatEvent>, ProviderError> {
        Ok(vec![
            ChatEvent::Delta {
                text: stdout.to_string(),
            },
            ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ])
    }

    /// Returns the variables the CLI needs from the environment. Only a test reads it; the
    /// runner passes on every variable not stripped anyway.
    fn env(&self) -> &[&str] {
        &[]
    }

    /// Returns the absolute paths of the CLI's own state directories, which a workspace-write
    /// jail makes writable besides the worktree, `$TMPDIR` and `/tmp`; one that does not exist is
    /// skipped. By default none.
    fn state_dirs(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}
