//! Holds what one CLI run needs (the invocation, the runner trait, the bytes exchanged). It
//! spawns nothing.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::Value;

use crate::ProviderError;

use super::claude_argv::{ClaudeCliEffort, ClaudeToolAccess};
use super::system_prompt_file::SystemPromptFile;

/// Everything a [`SubprocessRunner`] needs for one run.
#[derive(Clone, Debug)]
pub struct SubprocessInvocation {
    pub executable: String,
    pub model: String,
    /// Shared, not copied: the file goes when the last invocation holding it does.
    pub system: Option<Arc<SystemPromptFile>>,
    pub prompt: String,
    pub timeout: Duration,
    /// Names the environment variables removed from the child's environment, upper-cased; each
    /// matches a variable whatever its case.
    pub stripped_env: HashSet<String>,
    pub tools: ClaudeToolAccess,
    /// Passes `--bare`; see the builder doc for what it does to login.
    pub bare: bool,
    /// Passes `--effort`; `None` omits it.
    pub effort: Option<ClaudeCliEffort>,
    pub exclude_dynamic_sections: bool,
    /// Arguments appended after every other one.
    pub extra_args: Vec<String>,
    /// The directory the CLI's own tools work in and the jail's root; `None` uses the process
    /// cwd.
    pub cwd: Option<PathBuf>,
    /// The caller's sandbox policy, from `Caller::sandbox`; the runner tightens
    /// `DangerFullAccess` to a workspace-write jail.
    pub sandbox: tars_sandbox::SandboxPolicy,
}

impl SubprocessInvocation {
    /// Returns an invocation for a dialect other than claude, every claude-only field at its
    /// default.
    pub fn neutral(
        executable: String,
        model: String,
        prompt: String,
        timeout: Duration,
        stripped_env: HashSet<String>,
        cwd: Option<PathBuf>,
        sandbox: tars_sandbox::SandboxPolicy,
    ) -> Self {
        Self {
            executable,
            model,
            system: None,
            prompt,
            timeout,
            stripped_env,
            tools: ClaudeToolAccess::Disabled,
            bare: false,
            effort: None,
            exclude_dynamic_sections: false,
            extra_args: Vec::new(),
            cwd,
            sandbox,
        }
    }
}

/// Runs a CLI once and returns its output as one JSON value. However the run ends, it returns the
/// bytes exchanged with the CLI, so the attempt is recorded as it happened.
///
/// # Errors
///
/// A run past the invocation's timeout is killed and fails with `TimedOut`, not
/// `CliSubprocessDied`: the child did not die, the runner stopped it. A child that exits non-zero
/// or prints what the dialect cannot read fails with `CliSubprocessDied`.
#[async_trait]
pub trait SubprocessRunner: Send + Sync {
    async fn run(&self, invocation: SubprocessInvocation) -> Result<RunOutput, RunFailure>;
}

/// What a run exchanged with its delegate, as far as it got.
#[derive(Debug, Clone, Default)]
pub struct ChildIo {
    /// The argv spawned, which under a jail starts with the wrapper; `None` when the run failed
    /// before a spawn, and then the streams are empty too.
    pub argv: Option<Vec<String>>,
    /// The bytes written to stdin, as far as the write got; empty when the prompt is in the
    /// argv.
    pub stdin: Bytes,
    /// Stdout, as far as it was read.
    pub stdout: Bytes,
    /// Stderr, as far as it was read.
    pub stderr: Bytes,
}

impl ChildIo {
    /// Returns the argv `cmd` will spawn, program first.
    pub fn argv_of(cmd: &std::process::Command) -> Vec<String> {
        std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }
}

/// A run that produced its payload.
#[derive(Debug)]
pub struct RunOutput {
    pub payload: Value,
    pub exchanged: ChildIo,
}

/// A run that failed, with what it had exchanged by then.
#[derive(Debug)]
pub struct RunFailure {
    pub error: ProviderError,
    pub exchanged: ChildIo,
}

impl RunFailure {
    /// Returns a failure before anything was exchanged.
    pub fn before_exchange(error: ProviderError) -> Self {
        Self {
            error,
            exchanged: ChildIo::default(),
        }
    }
}
