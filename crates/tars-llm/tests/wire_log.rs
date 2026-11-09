//! Checks that every attempt a transport makes for a logged call becomes one `llm_wire` row with
//! the bytes sent and received, verbatim, and how the attempt ended.

use std::sync::Arc;
use std::time::Duration;
use tars_types::ProviderId;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::json;
use tokio::runtime::Handle;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tars_llm::ProviderError;
use tars_llm::provider::LlmProvider;
use tars_llm::provider::auth::{Auth, basic};
use tars_llm::provider::backends::anthropic::AnthropicProviderBuilder;
use tars_llm::provider::backends::cli::{
    AgentCliBackend, ChildIo, ClaudeCliDialect, ClaudeToolAccess, RunFailure, RunOutput,
    SubprocessInvocation, SubprocessRunner,
};
use tars_llm::provider::http_base::HttpClient;
use tars_llm::provider::wire::{
    SpawnedProcess, Transport, WireOutcome, WireRecorder, WireRow, WireTable,
};
use tars_melt::event::log::{LogSink, LogSource, MemLog, TenantScope, TimeWindow};
use tars_melt::event::wire::{WIRE_LOG_SINK, attempts};
use tars_types::{Caller, ChatRequest, LoggedCall, Pricing, ProviderProfile};

/// Returns the HTTP status an attempt recorded; `None` on a subprocess.
fn status(row: &WireRow) -> Option<u16> {
    match &row.transport {
        Transport::HttpSse { status } => *status,
        Transport::Subprocess { .. } => None,
    }
}

/// Returns the process a subprocess attempt spawned; `None` on HTTP.
fn spawned(row: &WireRow) -> Option<&SpawnedProcess> {
    match &row.transport {
        Transport::Subprocess { process } => process.as_ref(),
        Transport::HttpSse { .. } => None,
    }
}

fn argv(row: &WireRow) -> Option<Vec<String>> {
    spawned(row).map(|p| p.argv.clone())
}

fn stderr(row: &WireRow) -> Option<Bytes> {
    spawned(row).map(|p| p.stderr.clone())
}

const SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-x\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":3}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

/// A wire log in memory, and the sink to close before reading it.
struct Wire {
    log: MemLog<WireTable>,
    sink: Arc<dyn LogSink<WireTable>>,
}

impl Wire {
    fn new() -> Self {
        let log = MemLog::<WireTable>::new();
        let sink: Arc<dyn LogSink<WireTable>> =
            Arc::new(log.sink(WIRE_LOG_SINK, &Handle::current()));
        Self { log, sink }
    }

    fn recorder(&self) -> WireRecorder {
        WireRecorder::new(self.sink.clone())
    }

    /// Closes the sink and returns the attempts of `ctx`'s call.
    async fn attempts(&self, ctx: &Caller) -> Vec<WireRow> {
        self.sink.close().await.unwrap();
        let call = ctx.call.as_ref().expect("a logged call").id;
        attempts(&self.log, &ctx.tenant_id, call).await.unwrap()
    }
}

fn logged_ctx() -> Caller {
    let mut ctx = Caller::test_default();
    ctx.call = Some(LoggedCall::new(1_790_553_600_000).unwrap());
    ctx
}

fn anthropic(server: &MockServer, wire: WireRecorder) -> Arc<dyn LlmProvider> {
    AnthropicProviderBuilder::new(ProviderId::new("ant"), Auth::inline("k"))
        .base_url(server.uri())
        .wire(Some(wire))
        .build(HttpClient::default_arc().unwrap(), basic())
}

async fn serve(status: u16, body: impl Into<Vec<u8>>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("content-type", "text/event-stream")
                .set_body_bytes(body.into()),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn an_attempt_records_the_bytes_sent_and_received_as_they_were() {
    let server = serve(200, SSE).await;
    let wire = Wire::new();
    let ctx = logged_ctx();

    let resp = anthropic(&server, wire.recorder())
        .complete(ChatRequest::user("say hi"), "claude-x", ctx.clone())
        .await
        .unwrap();
    assert_eq!(resp.text, "hi");

    let rows = wire.attempts(&ctx).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let received = &server.received_requests().await.unwrap()[0];
    assert_eq!(row.request_bytes, Bytes::from(received.body.clone()));
    assert_eq!(row.response_bytes, Bytes::from_static(SSE.as_bytes()));
    assert_eq!(status(row), Some(200));
    assert!(!row.response_truncated);
    assert_eq!(row.outcome, WireOutcome::Completed);
    assert_eq!(row.attempt, 0);
    assert!(matches!(row.transport, Transport::HttpSse { .. }));
    assert_eq!(row.provider_id.as_str(), "ant");
    assert_eq!(row.model, "claude-x");
    assert_eq!(row.tenant, ctx.tenant_id);
    // The body still goes out as JSON.
    assert_eq!(
        received
            .headers
            .get("content-type")
            .map(|v| v.to_str().unwrap()),
        Some("application/json")
    );
}

#[tokio::test]
async fn an_error_response_records_its_raw_body_and_the_error_as_raised() {
    let body = br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    let server = serve(529, body.as_slice()).await;
    let wire = Wire::new();
    let ctx = logged_ctx();

    let err = anthropic(&server, wire.recorder())
        .complete(ChatRequest::user("hi"), "claude-x", ctx.clone())
        .await
        .unwrap_err();

    let rows = wire.attempts(&ctx).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(status(&rows[0]), Some(529));
    assert_eq!(rows[0].response_bytes, Bytes::from_static(body));
    assert!(!rows[0].response_truncated);
    assert_eq!(
        rows[0].outcome,
        WireOutcome::Failed {
            error: tars_utils::display_chain(&err)
        }
    );
}

#[tokio::test]
async fn an_error_body_past_the_cap_is_kept_to_the_cap_and_said_to_be_cut() {
    let server = serve(500, vec![b'x'; 20_000]).await;
    let wire = Wire::new();
    let ctx = logged_ctx();

    let _ = anthropic(&server, wire.recorder())
        .complete(ChatRequest::user("hi"), "claude-x", ctx.clone())
        .await
        .unwrap_err();

    let rows = wire.attempts(&ctx).await;
    assert_eq!(rows[0].response_bytes.len(), 8 * 1024);
    assert!(rows[0].response_truncated);
}

#[tokio::test]
async fn a_request_that_never_reaches_a_server_records_the_error_and_no_status() {
    // A port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let wire = Wire::new();
    let ctx = logged_ctx();
    let provider = AnthropicProviderBuilder::new(ProviderId::new("ant"), Auth::inline("k"))
        .base_url(format!("http://127.0.0.1:{port}"))
        .wire(Some(wire.recorder()))
        .build(HttpClient::default_arc().unwrap(), basic());

    let err = provider
        .complete(ChatRequest::user("hi"), "claude-x", ctx.clone())
        .await
        .unwrap_err();

    let rows = wire.attempts(&ctx).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(status(&rows[0]), None);
    assert!(rows[0].response_bytes.is_empty());
    assert!(
        !rows[0].request_bytes.is_empty(),
        "the request was attempted"
    );
    let WireOutcome::Failed { error } = &rows[0].outcome else {
        panic!("a failed attempt, not {:?}", rows[0].outcome);
    };
    assert!(error.starts_with("network: "), "{error}");
    assert!(error.contains("Connection refused"), "{error}");
    assert_eq!(error, &tars_utils::display_chain(&err));
}

#[tokio::test]
async fn a_stream_the_caller_drops_is_recorded_as_cancelled_with_what_arrived() {
    let server = serve(200, SSE).await;
    let wire = Wire::new();
    let ctx = logged_ctx();

    let mut stream = anthropic(&server, wire.recorder())
        .stream(ChatRequest::user("hi"), "claude-x", ctx.clone())
        .await
        .unwrap();
    stream.next().await.unwrap().unwrap();
    drop(stream);

    let rows = wire.attempts(&ctx).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, WireOutcome::Cancelled);
    assert!(
        SSE.as_bytes().starts_with(&rows[0].response_bytes),
        "what arrived is a prefix of what was served"
    );
}

#[tokio::test]
async fn a_call_no_log_stamped_records_no_attempt() {
    let server = serve(200, SSE).await;
    let wire = Wire::new();

    anthropic(&server, wire.recorder())
        .complete(ChatRequest::user("hi"), "claude-x", Caller::test_default())
        .await
        .unwrap();

    wire.sink.close().await.unwrap();
    let tenant = Caller::test_default().tenant_id;
    let scope = TenantScope::new(tenant, TimeWindow::all());
    assert!(wire.log.scan(&scope).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_stream_dropped_after_the_providers_end_is_recorded_as_completed() {
    let server = serve(200, SSE).await;
    let wire = Wire::new();
    let ctx = logged_ctx();

    let mut stream = anthropic(&server, wire.recorder())
        .stream(ChatRequest::user("hi"), "claude-x", ctx.clone())
        .await
        .unwrap();
    while let Some(ev) = stream.next().await {
        if matches!(ev.unwrap(), tars_types::ChatEvent::Finished { .. }) {
            break;
        }
    }
    drop(stream);

    let rows = wire.attempts(&ctx).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, WireOutcome::Completed);
}

#[tokio::test]
async fn an_http_attempt_has_no_stderr() {
    let server = serve(200, SSE).await;
    let wire = Wire::new();
    let ctx = logged_ctx();

    anthropic(&server, wire.recorder())
        .complete(ChatRequest::user("hi"), "claude-x", ctx.clone())
        .await
        .unwrap();

    assert_eq!(stderr(&wire.attempts(&ctx).await[0]), None);
}

#[tokio::test]
async fn an_http_attempt_has_no_argv() {
    let server = serve(200, SSE).await;
    let wire = Wire::new();
    let ctx = logged_ctx();

    anthropic(&server, wire.recorder())
        .complete(ChatRequest::user("hi"), "claude-x", ctx.clone())
        .await
        .unwrap();

    assert_eq!(argv(&wire.attempts(&ctx).await[0]), None);
}

struct Canned {
    exchanged: ChildIo,
    fails_with: Option<String>,
}

#[async_trait]
impl SubprocessRunner for Canned {
    async fn run(&self, _inv: SubprocessInvocation) -> Result<RunOutput, RunFailure> {
        let exchanged = self.exchanged.clone();
        match &self.fails_with {
            Some(stderr) => Err(RunFailure {
                error: ProviderError::CliSubprocessDied {
                    exit_code: Some(1),
                    stderr: stderr.clone(),
                },
                exchanged,
            }),
            None => Ok(RunOutput {
                payload: serde_json::from_slice(&exchanged.stdout).unwrap(),
                exchanged,
            }),
        }
    }
}

fn claude_backend(runner: Canned, wire: WireRecorder) -> Arc<AgentCliBackend> {
    let dialect = Arc::new(ClaudeCliDialect::new(
        "claude".into(),
        Duration::from_secs(30),
        ClaudeToolAccess::Disabled,
        false,
        None,
        true,
        Vec::new(),
    ));
    Arc::new(
        AgentCliBackend::new(
            ProviderId::new("claude_cli"),
            ProviderProfile::text_only_baseline(Pricing::default()),
            dialect,
            Arc::new(runner),
        )
        .wire(Some(wire)),
    )
}

fn exchanged(stdout: &[u8]) -> ChildIo {
    ChildIo {
        argv: Some(vec![
            "claude".into(),
            "-p".into(),
            "--model".into(),
            "opus".into(),
        ]),
        stdin: Bytes::from_static(b"hi"),
        stdout: Bytes::copy_from_slice(stdout),
        stderr: Bytes::from_static(b"warming up\n"),
    }
}

#[tokio::test]
async fn a_delegate_run_records_its_argv_stdin_and_stdout() {
    let stdout = json!({"result": "hello", "is_error": false, "usage": {"input_tokens": 1, "output_tokens": 1}})
        .to_string();
    let wire = Wire::new();
    let ctx = logged_ctx();
    let backend = claude_backend(
        Canned {
            exchanged: exchanged(stdout.as_bytes()),
            fails_with: None,
        },
        wire.recorder(),
    );

    backend
        .complete(ChatRequest::user("hi"), "opus", ctx.clone())
        .await
        .unwrap();

    let rows = wire.attempts(&ctx).await;
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].transport, Transport::Subprocess { .. }));
    assert_eq!(rows[0].request_bytes, Bytes::from_static(b"hi"));
    assert_eq!(
        argv(&rows[0]),
        Some(vec![
            "claude".to_owned(),
            "-p".to_owned(),
            "--model".to_owned(),
            "opus".to_owned()
        ])
    );
    assert_eq!(rows[0].response_bytes, Bytes::from(stdout));
    assert_eq!(status(&rows[0]), None);
    assert_eq!(rows[0].outcome, WireOutcome::Completed);
}

#[tokio::test]
async fn a_failed_delegate_run_records_its_stdout_and_the_error() {
    let wire = Wire::new();
    let ctx = logged_ctx();
    let backend = claude_backend(
        Canned {
            exchanged: exchanged(b"{\"partial\":"),
            fails_with: Some("You have hit your usage limit".into()),
        },
        wire.recorder(),
    );

    let err = backend
        .complete(ChatRequest::user("hi"), "opus", ctx.clone())
        .await
        .unwrap_err();

    let rows = wire.attempts(&ctx).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].response_bytes, Bytes::from_static(b"{\"partial\":"));
    assert_eq!(
        rows[0].outcome,
        WireOutcome::Failed {
            error: tars_utils::display_chain(&err)
        }
    );
    assert!(err.to_string().contains("You have hit your usage limit"));
}

#[tokio::test]
async fn a_delegate_runs_stderr_is_kept_as_it_was_written_whichever_way_it_ends() {
    for fails_with in [None, Some("You have hit your usage limit".to_owned())] {
        let wire = Wire::new();
        let ctx = logged_ctx();
        let stdout = json!({"result": "hello", "is_error": false}).to_string();
        let backend = claude_backend(
            Canned {
                exchanged: exchanged(stdout.as_bytes()),
                fails_with: fails_with.clone(),
            },
            wire.recorder(),
        );

        let _ = backend
            .complete(ChatRequest::user("hi"), "opus", ctx.clone())
            .await;

        assert_eq!(
            stderr(&wire.attempts(&ctx).await[0]),
            Some(Bytes::from_static(b"warming up\n")),
            "fails_with={fails_with:?}"
        );
    }
}

#[tokio::test]
async fn a_delegate_that_never_spawned_has_no_stderr() {
    let wire = Wire::new();
    let ctx = logged_ctx();
    let backend = claude_backend(
        Canned {
            exchanged: ChildIo::default(),
            fails_with: Some("`claude` not found in PATH".into()),
        },
        wire.recorder(),
    );

    let _ = backend
        .complete(ChatRequest::user("hi"), "opus", ctx.clone())
        .await;

    let rows = wire.attempts(&ctx).await;
    assert_eq!(stderr(&rows[0]), None);
    assert!(rows[0].request_bytes.is_empty());
    assert_eq!(argv(&rows[0]), None);
}

/// Returns a fresh directory and a script in it that prints `stdout` and `stderr` (as `printf`
/// formats) and exits with `code`.
#[cfg(unix)]
fn delegate(name: &str, stdout: &str, stderr: &str, code: i32) -> (std::path::PathBuf, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tars_wire_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    let path = dir.join("delegate.sh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nprintf '{stdout}'\nprintf '{stderr}' >&2\nexit {code}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path.to_string_lossy().into_owned())
}

/// Returns the shared runner of a plain-text CLI (agy) and an invocation of `executable` in
/// `cwd`.
#[cfg(unix)]
fn text_delegate_run(
    executable: String,
    cwd: &std::path::Path,
) -> (Arc<dyn SubprocessRunner>, SubprocessInvocation) {
    use tars_llm::provider::backends::cli::{
        AntigravityDialect, AntigravityEffort, SharedCliRunner,
    };
    let dialect = Arc::new(AntigravityDialect::new(
        executable.clone(),
        Duration::from_secs(30),
        AntigravityEffort::High,
    ));
    let inv = SubprocessInvocation::neutral(
        executable,
        "agy-model".into(),
        "the prompt".into(),
        Duration::from_secs(30),
        std::collections::HashSet::new(),
        Some(cwd.to_path_buf()),
        tars_sandbox::SandboxPolicy::default(),
    );
    (Arc::new(SharedCliRunner::new(dialect)), inv)
}

#[cfg(unix)]
#[tokio::test]
async fn the_real_runner_hands_back_stdout_and_stderr_byte_for_byte() {
    let (dir, script) = delegate("ok", "done\\n\\377", "a warning\\n", 0);
    let (runner, inv) = text_delegate_run(script, &dir);

    let out = runner.run(inv).await.unwrap();

    assert_eq!(out.exchanged.stdout, Bytes::from_static(b"done\n\xff"));
    assert_eq!(out.exchanged.stderr, Bytes::from_static(b"a warning\n"));
    let argv = out.exchanged.argv.expect("a spawned delegate has an argv");
    assert!(
        argv.iter().any(|a| a.ends_with("delegate.sh")),
        "the argv names the delegate: {argv:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn the_real_runner_hands_back_what_a_failed_delegate_printed() {
    let (dir, script) = delegate("failed", "partial answer", "limit reached\\n", 3);
    let (runner, inv) = text_delegate_run(script, &dir);

    let failure = runner.run(inv).await.unwrap_err();

    assert_eq!(
        failure.exchanged.stdout,
        Bytes::from_static(b"partial answer")
    );
    assert_eq!(
        failure.exchanged.stderr,
        Bytes::from_static(b"limit reached\n")
    );
    assert!(failure.error.to_string().contains("limit reached"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Returns a fresh directory and a script in it that saves its stdin to `got_stdin` and prints
/// `stdout`.
#[cfg(unix)]
fn stdin_keeping_delegate(name: &str, stdout: &[&str]) -> (std::path::PathBuf, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tars_wire_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    let path = dir.join("delegate.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncat > '{}'\nprintf '%s\\n' {}\n",
            dir.join("got_stdin").display(),
            stdout
                .iter()
                .map(|line| format!("'{line}'"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path.to_string_lossy().into_owned())
}

fn cli_backend(
    dialect: Arc<dyn tars_llm::provider::backends::cli::CliDialect>,
    wire: WireRecorder,
) -> Arc<AgentCliBackend> {
    use tars_llm::provider::backends::cli::SharedCliRunner;
    Arc::new(
        AgentCliBackend::new(
            ProviderId::new("cli"),
            ProviderProfile::text_only_baseline(Pricing::default()),
            dialect.clone(),
            Arc::new(SharedCliRunner::new(dialect)),
        )
        .wire(Some(wire)),
    )
}

#[cfg(unix)]
#[tokio::test]
async fn a_stdin_delegates_request_bytes_are_what_went_into_its_stdin() {
    use tars_llm::provider::backends::cli::{CodexCliDialect, CodexSandbox};
    let (dir, script) = stdin_keeping_delegate(
        "stdin",
        &[
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"hi"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0}}"#,
        ],
    );
    let wire = Wire::new();
    let ctx = logged_ctx().with_cwd(&dir);
    let dialect = Arc::new(CodexCliDialect::new(
        script.clone(),
        Duration::from_secs(30),
        CodexSandbox::ReadOnly,
        true,
    ));

    cli_backend(dialect, wire.recorder())
        .complete(
            ChatRequest::user("STDIN-MARKER-7"),
            "codex-model",
            ctx.clone(),
        )
        .await
        .unwrap();

    let rows = wire.attempts(&ctx).await;
    let got = std::fs::read(dir.join("got_stdin")).unwrap();
    assert!(!got.is_empty(), "the delegate read a prompt from stdin");
    assert_eq!(rows[0].request_bytes.as_ref(), got.as_slice());
    let argv = argv(&rows[0]).expect("a spawned delegate has an argv");
    assert!(argv.iter().any(|a| a.ends_with("delegate.sh")), "{argv:?}");
    assert!(
        argv.iter().all(|a| !a.contains("STDIN-MARKER-7")),
        "the prompt went in through stdin, not the argv: {argv:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn an_argv_delegates_request_bytes_are_empty_and_its_argv_carries_the_prompt() {
    use tars_llm::provider::backends::cli::{AntigravityDialect, AntigravityEffort};
    let (dir, script) = delegate("argv", "done", "", 0);
    let wire = Wire::new();
    let ctx = logged_ctx().with_cwd(&dir);
    let dialect = Arc::new(AntigravityDialect::new(
        script,
        Duration::from_secs(30),
        AntigravityEffort::High,
    ));

    cli_backend(dialect, wire.recorder())
        .complete(ChatRequest::user("ARGV-MARKER-9"), "agy-model", ctx.clone())
        .await
        .unwrap();

    let rows = wire.attempts(&ctx).await;
    assert!(rows[0].request_bytes.is_empty());
    let argv = argv(&rows[0]).expect("a spawned delegate has an argv");
    assert!(
        argv.iter().any(|a| a.contains("ARGV-MARKER-9")),
        "the prompt is in the argv: {argv:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
