//! The call-context law (Doc 06 §9): `RUN_CONTEXT` task-local + the
//! [`spawn_with_context`] helper.
//!
//! Inside Rust the per-operation [`RequestContext`] rides a
//! `tokio::task_local!` — established **once** at the operation entry
//! (`RUN_CONTEXT.scope(ctx, async { … }).await`), read implicitly by deep
//! calls and by the observability sink. Deep `pipeline.call` / `runtime`
//! never thread `ctx` by hand.
//!
//! **The footgun this kills:** a task-local does **not** propagate across
//! `tokio::spawn` — a detached job (one that must survive the command
//! returning) would silently lose the context. So every internal detached
//! task goes through [`spawn_with_context`], which captures the current
//! `RUN_CONTEXT` and re-scopes it inside the spawn. (`FuturesUnordered`
//! inside `run_plan` is the *same* task, so it inherits without help;
//! `spawn` does not.)
//!
//! Across language boundaries (PyO3 / napi / Tauri) the context is passed
//! **explicitly** and the binding re-establishes the scope — the task-local
//! is a Rust-internal elegance, never crossing FFI (Tasks 2/3).

use tokio::task::JoinHandle;

use crate::context::RequestContext;

tokio::task_local! {
    /// The current operation's [`RequestContext`] — ids only (tenant /
    /// session / trace / tags), never providers (§9: kept light; the
    /// handle carries the `Arc` providers). Established at the operation
    /// entry via `RUN_CONTEXT.scope(ctx, fut).await`.
    pub static RUN_CONTEXT: RequestContext;
}

/// Spawn a detached task that inherits the current [`RUN_CONTEXT`].
///
/// A bare `tokio::spawn` starts a fresh task with **no** task-local, so the
/// caller's context would be lost. This captures the current context and
/// re-scopes it inside the spawned task, so detached jobs (the async agent
/// job that outlives the command) keep the same tenant / session / trace.
///
/// Must be called from within a `RUN_CONTEXT` scope (it reads the current
/// context); calling it outside a scope panics, same as `RUN_CONTEXT.with`.
pub fn spawn_with_context<F>(fut: F) -> JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let ctx = RUN_CONTEXT.with(|c| c.clone());
    tokio::spawn(async move { RUN_CONTEXT.scope(ctx, fut).await })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TenantId;

    /// A deep call reads the context without it being threaded through.
    #[tokio::test]
    async fn deep_call_reads_context_implicitly() {
        let ctx = RequestContext::test_default();
        let seen = RUN_CONTEXT
            .scope(ctx, async {
                // No ctx argument threaded here — read the task-local.
                RUN_CONTEXT.with(|c| c.tenant_id.clone())
            })
            .await;
        assert_eq!(seen, TenantId::new("tenant-test"));
    }

    /// A `spawn_with_context` job re-establishes the same context; a bare
    /// `tokio::spawn` would not (the task-local would be absent).
    #[tokio::test]
    async fn spawned_job_rescopes_context() {
        let mut ctx = RequestContext::test_default();
        ctx.tenant_id = TenantId::new("acme");
        let handle = RUN_CONTEXT
            .scope(ctx, async {
                spawn_with_context(async { RUN_CONTEXT.with(|c| c.tenant_id.clone()) })
            })
            .await;
        let tenant = handle.await.expect("spawned task joins");
        assert_eq!(tenant, TenantId::new("acme"));
    }
}
