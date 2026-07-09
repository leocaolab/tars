//! **Native-agent security integration test** (tracking doc §3 "reviewer /
//! native" path, §5 M5/M6, D8) — the tars-tools / deepseek-mock path.
//!
//! Wires a *real agent* end-to-end and proves the OS sandbox confines it:
//!
//! ```text
//!   MockLlmProvider  →  Session's real tool loop  →  ToolRegistry.dispatch
//!   (canned tool-call)     (drive_with_tools)         →  BashTool.execute
//!                                                       →  ctx.sandbox.wrap
//!                                                       →  macOS Seatbelt jail
//! ```
//!
//! The mock LLM (`MockProvider` from `tars-provider`, the MockLlmProvider of the
//! two-mocks plan) emits a `bash.run` tool call whose command tries to
//! `echo pwned > <abs path OUTSIDE the worktree>`. The `Session` — the SAME
//! driver a live deepseek agent uses — dispatches it through the real
//! `ToolRegistry`, which runs the real `BashTool`, which wraps `sh -c` in the
//! real `SandboxPolicy::workspace_write(worktree)` (macOS `sandbox-exec`). The
//! escape write is denied by the OS; a second tool call writing INSIDE the
//! worktree succeeds. Nothing here is hand-synthesized: mock LLM → real loop →
//! real sandbox.
//!
//! **Why `Session` (not `WorkerAgent`).** Only `Session` lets the caller supply
//! a full `ToolContext` (including `sandbox`) via `SessionOptions::tool_ctx`,
//! which it clones into every `registry.dispatch`. `WorkerAgent::drive_with_tools`
//! currently builds its per-call `ToolContext` with `..Default::default()` for
//! `sandbox` (unrestricted) and doesn't thread one from `AgentContext`, so it
//! can't carry a confining policy without a source change (out of scope here).
//! `Session` is the faithful native-agent harness for the sandbox seam.
//!
//! macOS-gated: it spawns the real `sandbox-exec`. On other platforms the test
//! is compiled out (Linux `bwrap` confinement is a separate validation debt,
//! tracking doc §7).

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::sync::Arc;

use tars_pipeline::LlmService;
use tars_provider::{CannedResponse, LlmProvider, MockProvider};
use tars_runtime::{Budget, Session, SessionOptions};
use tars_tools::{SandboxPolicy, ToolContext, ToolRegistry, builtins::BashTool};
use tars_types::{
    Capabilities, ChatEvent, ModelHint, Pricing, StopReason, Usage,
};

/// Build the `LlmService` a `Session` drives, from a mock `LlmProvider` —
/// same `ProviderService` → `Pipeline` wrapping the live path uses.
fn build_llm(provider: Arc<dyn LlmProvider>) -> LlmService {
    let inner: LlmService = LlmService::of(provider, "gpt-4o");
    LlmService::builder_with_inner(inner).build()
}

/// One canned turn: the model emits a single `bash.run` tool call carrying
/// `command`, then finishes with `ToolUse` (asking the runtime to run it).
fn bash_call_turn(call_id: &str, command: &str) -> CannedResponse {
    CannedResponse::Sequence(vec![
        ChatEvent::started("mock-deepseek"),
        ChatEvent::ToolCallStart {
            index: 0,
            id: call_id.into(),
            name: "bash.run".into(),
        },
        ChatEvent::ToolCallEnd {
            index: 0,
            id: call_id.into(),
            parsed_args: serde_json::json!({ "command": command }),
            thought_signature: None,
        },
        ChatEvent::Finished {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ])
}

/// Final text-only turn — ends the tool loop; `Session::send_text` returns it.
fn final_text_turn(text: &str) -> CannedResponse {
    CannedResponse::Sequence(vec![
        ChatEvent::started("mock-deepseek"),
        ChatEvent::Delta { text: text.into() },
        ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ])
}

fn bash_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register_owned(BashTool::new()).unwrap();
    reg
}

/// A native agent (mock LLM + tars's own BashTool) tries to escape its worktree
/// via a `bash` tool call; the OS sandbox blocks the outside write and permits
/// the inside one. Proves the AGENT-level path (mock LLM → real Session tool
/// loop → real registry/tool → real sandbox), not the tool in isolation.
#[tokio::test]
async fn native_agent_bash_escape_is_blocked_by_sandbox() {
    // Real paths: macOS Seatbelt matches the canonical path (`/tmp` →
    // `/private/tmp`), so both the worktree and the escape target are rooted
    // under a canonicalized temp base — else the jail matches nothing.
    let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let worktree = base.join(format!("tars_native_agent_wt_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&worktree);
    std::fs::create_dir_all(&worktree).unwrap();
    let worktree = std::fs::canonicalize(&worktree).unwrap();

    // The escape target lives OUTSIDE the worktree (sibling under the temp
    // base). It must not exist before the run.
    let outside = base.join(format!("tars_native_agent_escape_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&outside);

    // Mock LLM: (1) try to write OUTSIDE the worktree, (2) write INSIDE the
    // worktree (relative to cwd), (3) finish. Replayed one per Session tool
    // round-trip via `with_responses`.
    let provider: Arc<dyn LlmProvider> = MockProvider::with_responses(
        "mock-deepseek",
        vec![
            bash_call_turn(
                "call_escape",
                &format!("echo pwned > {}", outside.display()),
            ),
            bash_call_turn("call_inside", "echo ok > inside.txt"),
            final_text_turn("done"),
        ],
    );

    // The confining policy, threaded through the real ToolContext the Session
    // hands every dispatch: write-jail rooted at the worktree, cwd = worktree.
    let tool_ctx = ToolContext {
        cwd: Some(worktree.clone()),
        sandbox: SandboxPolicy::workspace_write(&worktree),
        ..Default::default()
    };

    let mut session = Session::new(
        build_llm(provider),
        Capabilities::text_only_baseline(Pricing::default()),
        SessionOptions {
            system: "you are a coding agent with a bash tool".into(),
            budget: Budget::Chars(usize::MAX / 2),
            tools: Some(bash_registry()),
            tool_ctx,
            default_max_output_tokens: None,
            model: ModelHint::Explicit("mock-deepseek".into()),
        },
    );

    let reply = session
        .send_text("do the work", None)
        .await
        .expect("session should drive the tool loop to a final answer");

    // The escape write was denied by the OS sandbox — the file never appeared
    // outside the worktree. This is the security-critical assertion.
    assert!(
        !outside.exists(),
        "sandbox MUST block the native agent's bash write outside the worktree \
         (found {})",
        outside.display()
    );

    // The in-worktree write went through — the jail confines, it doesn't brick
    // the tool. Proves the sandbox is scoped, not a blanket deny.
    assert!(
        worktree.join("inside.txt").exists(),
        "write INSIDE the worktree must succeed through the same sandboxed path"
    );

    assert_eq!(reply, "done", "the agent loop ran to the final turn");

    let _ = std::fs::remove_dir_all(&worktree);
    let _ = std::fs::remove_file(&outside);
}
