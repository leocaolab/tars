//! Carries what one LLM call is made under through every pipeline layer down to the provider:
//! its ids, cancellation and deadline, and the working directory and sandbox a subprocess
//! provider runs in. What a layer measures goes back with the call's result, not on the context.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

pub use tokio_util::sync::CancellationToken;

use crate::ids::{CallId, CallIdError, SessionId, TenantId, TraceId};
use crate::task::Millis;

/// One logged call's identity, carried down to the transport so every
/// attempt the transport makes is recorded under the call's id.
///
/// A retry is a fresh `provider.stream` with a clone of the context, so the
/// attempt count is shared: clones hold the same counter, and only
/// [`Self::next_attempt`] — called by the transport as an attempt starts —
/// moves it.
#[derive(Clone, Debug)]
pub struct LoggedCall {
    pub id: CallId,
    attempts: Arc<AtomicU32>,
}

impl LoggedCall {
    /// Returns the stamp of a call that starts at `at` (epoch ms, read by the shell).
    ///
    /// # Errors
    ///
    /// Returns `TimeOutOfRange` when `at` is outside a UUIDv7 timestamp.
    pub fn new(at: Millis) -> Result<Self, CallIdError> {
        Ok(Self {
            id: CallId::new_v7(at)?,
            attempts: Arc::new(AtomicU32::new(0)),
        })
    }

    /// Returns the number of the attempt starting now: 0 for the first, then 1, 2 …
    pub fn next_attempt(&self) -> u32 {
        self.attempts.fetch_add(1, Ordering::Relaxed)
    }
}

/// The session of [`Caller::test_default`], a fixed version-7 UUID.
#[cfg(any(test, feature = "testutil"))]
pub const TEST_SESSION: &str = "01900000-0000-7000-8000-000000000000";

#[derive(Clone, Debug)]
pub struct Caller {
    pub trace_id: TraceId,
    pub tenant_id: TenantId,
    /// The session this call belongs to; `None` for a call made outside any session (a CLI run,
    /// a queued task, a binding call that named none).
    pub session_id: Option<SessionId>,
    /// The hard deadline; `None` means none.
    pub deadline: Option<Instant>,
    /// Cooperative cancellation that anyone holding it can trigger; a long await in an adapter
    /// selects against `cancel.cancelled()`.
    pub cancel: CancellationToken,
    /// Free-form values a middleware passes to inner layers. Lock it through
    /// [`Caller::read_attributes`] / [`Caller::write_attributes`], which recover
    /// a poisoned lock.
    pub attributes: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    /// Free-form cohort tags, copied onto the call's logged event so a query can filter on them
    /// (`WHERE 'dogfood_2026_05_08' = ANY(tags)`).
    pub tags: Vec<String>,
    /// The working directory of a provider that spawns a subprocess with its own tools
    /// (`claude_cli` running `claude -p --tools default`), so the agent's Read / Edit / Bash work
    /// in the intended tree; `None` inherits the parent process's cwd.
    pub cwd: Option<PathBuf>,
    /// The OS confinement of a provider that spawns a subprocess (the CLI delegates); HTTP
    /// providers ignore it. The caller sets it with [`Self::with_sandbox`]; a context built
    /// without one is unconfined (`SandboxPolicy::default`, `DangerFullAccess`).
    pub sandbox: tars_sandbox::SandboxPolicy,
    /// The logged call this request is, stamped by `EventEmitterMiddleware`
    /// before it runs the call. `None` when no call log is wired: the
    /// transport then records no wire rows either.
    pub call: Option<LoggedCall>,
}

impl Caller {
    /// Returns a context for a call made on `tenant`'s behalf: the given trace, no session, no
    /// deadline, unconfined. Only the caller knows the tenant, so there is no constructor
    /// without one:
    ///
    /// ```compile_fail
    /// let _ = tars_types::Caller::personal(tars_types::TraceId::new("t"));
    /// ```
    pub fn new(tenant: TenantId, trace_id: TraceId) -> Self {
        Self {
            trace_id,
            tenant_id: tenant,
            session_id: None,
            deadline: None,
            cancel: CancellationToken::new(),
            attributes: Arc::new(RwLock::new(HashMap::new())),
            tags: Vec::new(),
            cwd: None,
            sandbox: tars_sandbox::SandboxPolicy::default(),
            call: None,
        }
    }

    /// Returns a context for tests: tenant `tenant-test`, trace `trace-test`, session
    /// [`TEST_SESSION`]. It exists only in this crate's tests and with the `testutil` feature.
    #[cfg(any(test, feature = "testutil"))]
    pub fn test_default() -> Self {
        Self {
            session_id: Some(
                TEST_SESSION
                    .parse()
                    .expect("TEST_SESSION is a version-7 UUID literal"),
            ),
            ..Self::new(TenantId::new("tenant-test"), TraceId::new("trace-test"))
        }
    }

    /// Returns the context as a call of `session`.
    pub fn with_session(mut self, session: SessionId) -> Self {
        self.session_id = Some(session);
        self
    }

    /// Returns the context with [`Self::cwd`] set.
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Returns the context with [`Self::sandbox`] set.
    pub fn with_sandbox(mut self, sandbox: tars_sandbox::SandboxPolicy) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Returns the same caller's context for a new call, with no call stamp: a stamp belongs to
    /// one call, and the next call is stamped when it runs.
    pub fn for_call(&self) -> Self {
        Self {
            call: None,
            ..self.clone()
        }
    }

    /// Returns the context with `tags` replacing its tags.
    pub fn with_tags<S, I>(mut self, tags: I) -> Self
    where
        S: Into<String>,
        I: IntoIterator<Item = S>,
    {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Read-locks the attribute map, recovering a poisoned lock. The map is plain data with no
    /// invariant a panicking holder could break, so one middleware's panic is not passed on to
    /// every later reader.
    pub fn read_attributes(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, HashMap<String, serde_json::Value>> {
        self.attributes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write-locks the attribute map, recovering a poisoned lock as
    /// [`Self::read_attributes`] does.
    pub fn write_attributes(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, HashMap<String, serde_json::Value>> {
        self.attributes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Returns whether the call was cancelled or its deadline has passed.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled() || self.is_deadline_exceeded()
    }

    /// Returns whether a deadline is set and the clock has reached it, for a caller that tells a
    /// timeout from a cancellation. It reads the clock on every call, so `false` means only "not
    /// yet".
    pub fn is_deadline_exceeded(&self) -> bool {
        match self.deadline {
            Some(d) => Instant::now() >= d,
            None => false,
        }
    }

    /// Returns the wall-clock time left before the [`deadline`](Self::deadline), `Some(ZERO)` once
    /// it has passed, or `None` when the caller set none.
    ///
    /// A provider that owns a resource outliving its future, such as a subprocess, must honor it,
    /// because dropping the future does not kill the child; an HTTP provider's dropped future
    /// already closes the connection.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Returns the wall-clock budget of one provider call: the caller's
    /// [`remaining`](Self::remaining) when it set a deadline, else `default` (the provider's
    /// configured `timeout_secs`). The configured timeout is a default, not a ceiling: a caller's
    /// longer deadline buys a longer run.
    pub fn call_budget(&self, default: Duration) -> Duration {
        self.remaining().unwrap_or(default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamps_clones_count_one_calls_attempts() {
        let stamp = LoggedCall::new(1_790_000_000_000).unwrap();
        let retry = stamp.clone();

        assert_eq!(stamp.next_attempt(), 0);
        assert_eq!(retry.next_attempt(), 1);
        assert_eq!(stamp.next_attempt(), 2);
        assert_eq!(retry.id, stamp.id);
    }

    #[test]
    fn the_next_calls_context_carries_no_stamp() {
        let mut ctx = Caller::test_default();
        ctx.call = Some(LoggedCall::new(1_790_000_000_000).unwrap());

        assert!(ctx.for_call().call.is_none());
    }
}
