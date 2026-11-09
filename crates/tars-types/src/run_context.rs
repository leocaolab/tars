//! Carries the current operation's [`Caller`] in the `RUN_CONTEXT` task-local, set once
//! at the operation's entry (`RUN_CONTEXT.scope(ctx, fut).await`) so that code below it can read
//! the caller without a `ctx` argument. The node and Python bindings set it; no code in this
//! repository reads it yet.
//!
//! A task-local does not cross `tokio::spawn`, so a detached task started with a bare spawn
//! loses the context; spawn it with [`spawn_with_context`]. Futures polled inside the same task
//! (`FuturesUnordered`) keep it. It does not cross FFI either: the PyO3 / napi / Tauri bindings
//! take the context as an explicit argument and set the scope themselves.
//!
//! [`Caller::tenant_id`] is the partition key and always comes from the caller: there is
//! no default tenant or session. This module has no per-tenant registry and no authorization.

use tokio::task::JoinHandle;

use crate::providers::context::Caller;

tokio::task_local! {
    /// The current operation's [`Caller`]: who is calling, not which provider to call.
    pub static RUN_CONTEXT: Caller;
}

/// Spawns `fut` with the caller's `RUN_CONTEXT` set inside it, so a detached job that outlives
/// its command keeps the same tenant, session and trace.
///
/// # Panics
///
/// Panics when called outside a `RUN_CONTEXT` scope, as `RUN_CONTEXT.with` does.
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

    #[tokio::test]
    async fn deep_call_reads_context_implicitly() {
        let ctx = Caller::test_default();
        let seen = RUN_CONTEXT
            .scope(ctx, async { RUN_CONTEXT.with(|c| c.tenant_id.clone()) })
            .await;
        assert_eq!(seen, TenantId::new("tenant-test"));
    }

    #[tokio::test]
    async fn spawned_job_rescopes_context() {
        let mut ctx = Caller::test_default();
        ctx.tenant_id = TenantId::new("acme");
        let tenant = RUN_CONTEXT
            .scope(ctx, async {
                spawn_with_context(async { RUN_CONTEXT.with(|c| c.tenant_id.clone()) })
                    .await
                    .expect("spawned task joins")
            })
            .await;
        assert_eq!(tenant, TenantId::new("acme"));
    }

    #[tokio::test]
    async fn tenant_id_is_the_partition_key_and_flows_through_spawn() {
        let mut ctx = Caller::test_default();
        ctx.tenant_id = TenantId::new("acme-corp");
        let joined = RUN_CONTEXT
            .scope(ctx, async {
                assert_eq!(
                    RUN_CONTEXT.with(|c| c.tenant_id.clone()),
                    TenantId::new("acme-corp")
                );
                spawn_with_context(async { RUN_CONTEXT.with(|c| c.tenant_id.clone()) })
                    .await
                    .unwrap()
            })
            .await;
        assert_eq!(joined, TenantId::new("acme-corp"));
    }
}
