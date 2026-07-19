//! Proof that [`LlmAgent`] decides **through [`tars_pipeline::LlmService`]** (not raw HTTP) and
//! that a model tool call becomes an [`Intent`] that drives the same reconcile loop to a fixed
//! point. The provider is a [`MockProvider`] replaying a canned tool-call stream, so the test is
//! hermetic — but the path is the real one: `LlmService::builder(...).build()` → `service.call`
//! → drain `LlmEventStream` → parse `ToolCallEnd` → `Intent`.

use tars_agent2::{File, LlmAgent, Runtime, ShellCheck, Spec, World};
use tars_pipeline::LlmService;
use tars_provider::backends::mock::{CannedResponse, MockProvider};
use tars_types::{ChatEvent, RequestContext, StopReason, Usage};

#[tokio::test]
async fn llm_agent_tool_call_drives_reconcile_to_fixed_point() {
    let dir = tempfile::tempdir().unwrap();
    let status_path = dir.path().join("status.txt");
    std::fs::write(&status_path, "red").unwrap();

    // The model's canned turn: call the `write_status` tool with {"content":"green"}.
    let tool_turn = CannedResponse::Sequence(vec![
        ChatEvent::started("mock-model"),
        ChatEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "write_status".into(),
        },
        ChatEvent::ToolCallEnd {
            index: 0,
            id: "call_1".into(),
            parsed_args: serde_json::json!({"content": "green"}),
            thought_signature: None,
        },
        ChatEvent::Finished {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ]);
    // After the write the loop converges before another call; a final no-tool turn is a safe
    // fallback if it ever cranks again.
    let final_turn = CannedResponse::Sequence(vec![
        ChatEvent::started("mock-model"),
        ChatEvent::Delta { text: "done".into() },
        ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]);

    let provider = MockProvider::with_responses("mock", vec![tool_turn, final_turn]);

    // THE construction under test: build the pipeline service via the builder.
    let service = LlmService::builder(provider, "mock-model").build();

    let agent = LlmAgent::new(
        service,
        RequestContext::test_default(),
        "You fix the world. Call write_status with the required content to make the check green.",
    )
    .bind_tool(
        "write_status",
        "Overwrite status.txt with the given content.",
        serde_json::json!({
            "type": "object",
            "properties": {"content": {"type": "string"}},
            "required": ["content"]
        }),
        "status",
        "write",
    );

    let script = format!("grep -qx green {}", status_path.to_string_lossy());
    let spec = Spec::new().with(ShellCheck::new("status-green", "sh", ["-c", &script], dir.path()));
    let mut world = World::new().with(File::open("status", &status_path).unwrap());
    assert!(!world.converged(&spec), "starts red");

    let mut agent = agent;
    let outcome = Runtime::new(8).anneal(&mut world, &spec, &mut agent).await;

    assert!(
        outcome.converged(),
        "LlmAgent (via LlmService) must drive the loop to the fixed point, got {outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&status_path).unwrap(),
        "green",
        "the model's tool call must have produced a real File write"
    );
}
