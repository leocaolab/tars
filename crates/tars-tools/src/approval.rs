//! [`ApprovalSink`] — the human-in-the-loop channel that makes `Ask` real.
//!
//! When the dispatch gate sees a tool whose [`ToolDecision`](crate::ToolDecision)
//! is `Ask`, it asks an `ApprovalSink` (e.g. the Codex-TUI approval widget) to
//! allow or deny. With **no** sink (headless / `tars-cli`), `Ask` is treated as
//! `Deny` — fail closed (Doc 23 NFR-2).

use async_trait::async_trait;

/// A pending approval the runtime surfaces to a human.
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    /// Tool name being requested (e.g. `bash.run`).
    pub tool: String,
    /// One-line, human-readable summary of what's about to happen.
    pub summary: String,
    /// The raw arguments — show them; never hide intent behind a summary.
    pub args: serde_json::Value,
}

/// The human's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

/// Surfaces an [`ApprovalRequest`] to a human and awaits the answer. The runtime
/// drives this against `ctx.cancel`, so a dropped/cancelled turn aborts the
/// await rather than blocking forever.
#[async_trait]
pub trait ApprovalSink: Send + Sync {
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision;
}

/// The fail-closed default: deny every `Ask`. Used when no human channel exists
/// (headless / `tars-cli` without a prompt). An `Ask` with no sink is denied by
/// the gate directly; this is the explicit, registerable form of the same.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAllSink;

#[async_trait]
impl ApprovalSink for DenyAllSink {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deny_all_sink_denies_everything() {
        let d = DenyAllSink
            .request(ApprovalRequest {
                tool: "bash.run".into(),
                summary: "rm -rf /".into(),
                args: serde_json::json!({"cmd": "rm -rf /"}),
            })
            .await;
        assert_eq!(d, ApprovalDecision::Deny);
    }
}
