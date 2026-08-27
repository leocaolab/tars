//! Per-request context shared across pipeline layers.
//!
//! Deliberately minimal at the Provider layer. Full `RequestContext`
//! (with budget handle, attributes, etc.) lives in `tars-pipeline`;
//! providers only need IDs + cancel + deadline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

pub use tokio_util::sync::CancellationToken;

use crate::ids::{PrincipalId, SessionId, TenantId, TraceId};
use crate::telemetry::{SharedTelemetry, new_shared_telemetry};
use crate::validation::{SharedValidationOutcome, new_shared_validation_outcome};

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub trace_id: TraceId,
    pub tenant_id: TenantId,
    pub session_id: SessionId,
    pub principal_id: PrincipalId,
    /// Hard deadline. None = no deadline (rare in production).
    pub deadline: Option<Instant>,
    /// Cooperative cancellation. Anyone holding this can cancel; long
    /// awaits in adapters must `select!` against `cancel.cancelled()`.
    pub cancel: CancellationToken,
    /// Free-form attributes used by middleware to pass values to inner
    /// layers without bloating the strongly-typed fields.
    ///
    /// Prefer the poison-recovering [`RequestContext::read_attributes`] /
    /// [`RequestContext::write_attributes`] accessors over locking this
    /// field directly, so a panic in one middleware doesn't cascade into
    /// `unwrap()`-on-poison panics across the whole request path.
    pub attributes: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    /// Per-call telemetry accumulator written by middleware and read
    /// by the caller after the response stream completes. See
    /// [`crate::telemetry::TelemetryAccumulator`]. Always present —
    /// middleware writes are unconditional, callers ignore the slot
    /// if they don't need it.
    pub telemetry: SharedTelemetry,
    /// Per-call validation outcome side-channel. `ValidationMiddleware`
    /// writes the aggregated summary + (if any Filter ran) the
    /// post-Filter `ChatResponse`. Caller reads after stream drain
    /// and either uses the filtered response in place of the streamed
    /// one, or substitutes `summary` onto the response builder.
    /// See [`crate::validation::SharedValidationOutcome`].
    pub validation_outcome: SharedValidationOutcome,
    /// Free-form cohort tags. Propagated to `PipelineEvent.tags` so
    /// SQL rollups can `WHERE 'dogfood_2026_05_08' = ANY(tags)`.
    /// See Doc 17 §4 (cohort).
    ///
    /// Caller convenience: [`RequestContext::with_tags`] returns a
    /// new context with these set; usually called once at session /
    /// batch entry, propagated unchanged through the call.
    pub tags: Vec<String>,
    /// Working directory for the request. Native-agent providers that
    /// spawn a subprocess with its OWN tools (e.g. `claude_cli` running
    /// `claude -p` with `--tools default`) set the subprocess
    /// `current_dir` to this, so the agent's Read/Edit/Bash operate in
    /// the intended tree (the fix worktree) rather than arc's process
    /// cwd. `None` = inherit the parent process cwd.
    ///
    /// Set via [`RequestContext::with_cwd`]; threaded from the worker's
    /// `AgentContext.cwd`.
    pub cwd: Option<PathBuf>,
    /// OS-confinement policy for providers that spawn a subprocess (the CLI
    /// delegates — `claude_cli`/`gemini_cli`/`codex_cli`/`opencode`/
    /// `antigravity`). Resolved once from the `[sandbox]` TOML section +
    /// `--sandbox` flag (see `tars-config::resolve_policy`) and threaded here
    /// from the worker's `AgentContext.sandbox`, exactly like [`Self::cwd`].
    /// HTTP providers ignore it (no subprocess to jail).
    ///
    /// Default [`SandboxPolicy::default`] = `DangerFullAccess` = unconfined =
    /// today's behaviour, so a context built without a policy preserves the
    /// pre-G10 default. The legacy `TARS_CLAUDE_SANDBOX=1` env gate still
    /// forces a workspace-write jail when this stays unconfined (back-compat).
    ///
    /// Set via [`RequestContext::with_sandbox`].
    pub sandbox: tars_sandbox::SandboxPolicy,
}

impl RequestContext {
    /// A single-user ("personal mode") context: the given trace, a
    /// `"local"` tenant / session / principal, no deadline, fresh
    /// telemetry + validation handles. For local, single-tenant
    /// frontends (the personal-mode HTTP server, CLI). There is **no
    /// IAM / audit** here — a multi-tenant server must instead build a
    /// context from a resolved [`crate::Principal`].
    pub fn personal(trace_id: TraceId) -> Self {
        Self {
            trace_id,
            tenant_id: TenantId::new("local"),
            session_id: SessionId::new("local"),
            principal_id: PrincipalId::new("local"),
            deadline: None,
            cancel: CancellationToken::new(),
            attributes: Arc::new(RwLock::new(HashMap::new())),
            telemetry: new_shared_telemetry(),
            validation_outcome: new_shared_validation_outcome(),
            tags: Vec::new(),
            cwd: None,
            sandbox: tars_sandbox::SandboxPolicy::default(),
        }
    }

    /// A test/dev context — fresh trace, no deadline, no real principal.
    /// **Do not use in production** — there's no IAM/audit attached.
    pub fn test_default() -> Self {
        Self {
            trace_id: TraceId::new("trace-test"),
            tenant_id: TenantId::new("tenant-test"),
            session_id: SessionId::new("session-test"),
            principal_id: PrincipalId::new("principal-test"),
            deadline: None,
            cancel: CancellationToken::new(),
            attributes: Arc::new(RwLock::new(HashMap::new())),
            telemetry: new_shared_telemetry(),
            validation_outcome: new_shared_validation_outcome(),
            tags: Vec::new(),
            cwd: None,
            sandbox: tars_sandbox::SandboxPolicy::default(),
        }
    }

    /// Consume `self` and return it with `cwd` set (builder-style move).
    /// Native-agent providers spawn their subprocess with this as the
    /// `current_dir`; threaded from the worker's `AgentContext.cwd`.
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Consume `self` and return it with the OS-confinement `sandbox` policy
    /// set (builder-style move). Subprocess-spawning providers (CLI delegates)
    /// jail their spawn with this; threaded from the worker's
    /// `AgentContext.sandbox`.
    pub fn with_sandbox(mut self, sandbox: tars_sandbox::SandboxPolicy) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Consume `self` and return it with `tags` set (builder-style
    /// move — the input binding is moved, not cloned; `RequestContext`
    /// is `Clone`, so clone first if you need to keep the original).
    /// Convenience for batch runners: build one `ctx` then call
    /// `.with_tags(["batch_X"])` before each request.
    pub fn with_tags<S, I>(mut self, tags: I) -> Self
    where
        S: Into<String>,
        I: IntoIterator<Item = S>,
    {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Read-lock the attribute map, recovering from a poisoned lock.
    ///
    /// The attribute map is plain data — a panic by a prior lock holder
    /// does not leave it in an invariant-violating state — so we recover
    /// the guard via `PoisonError::into_inner` rather than propagating
    /// the poison and turning one middleware panic into a cascade of
    /// `unwrap()`-on-poison panics across the request path.
    pub fn read_attributes(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, HashMap<String, serde_json::Value>> {
        self.attributes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write-lock the attribute map, recovering from a poisoned lock.
    /// See [`Self::read_attributes`] for why poison is recovered here.
    pub fn write_attributes(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, HashMap<String, serde_json::Value>> {
        self.attributes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled() || self.is_deadline_exceeded()
    }

    /// True iff a deadline is set and `Instant::now()` has passed it.
    /// Kept separate from `is_cancelled()` for callers that want to
    /// distinguish a hard timeout from explicit caller cancellation.
    ///
    /// Re-samples the clock on every call by design: it is a live
    /// predicate on the current time, not a latched flag. Two calls
    /// straddling the deadline correctly return `false` then `true`;
    /// callers must treat a `false` as "not expired *as of now*", which
    /// is the only sound contract for a wall-clock deadline.
    pub fn is_deadline_exceeded(&self) -> bool {
        match self.deadline {
            Some(d) => Instant::now() >= d,
            None => false,
        }
    }

    /// Wall-clock budget left for this call, or `None` when the caller set no
    /// [`deadline`](Self::deadline).
    ///
    /// This is the **parameter** half of a provider's time bound: the caller
    /// says how long *this* unit of work may take. The provider's configured
    /// timeout (`[providers.X] timeout_secs`) is only the default used when the
    /// caller says nothing. A provider that owns a resource outliving the future
    /// — a subprocess — MUST honor this, because dropping the future does not
    /// kill its child. HTTP providers may honor it; dropping their future
    /// already closes the connection.
    ///
    /// Returns `Some(Duration::ZERO)` once the deadline has passed, so a caller
    /// that spawns on a blown budget fails immediately rather than running a
    /// full call it has no time for.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// The wall-clock budget one provider call gets: the caller's
    /// [`remaining`](Self::remaining) if it set a deadline, else `default` —
    /// the provider's configured `timeout_secs`.
    ///
    /// The caller **wins** when it speaks: a longer deadline buys a longer run
    /// (a reconcile that legitimately takes 20 minutes), a shorter one cuts the
    /// call off early. Config is the default, not a ceiling.
    pub fn call_budget(&self, default: Duration) -> Duration {
        self.remaining().unwrap_or(default)
    }
}
