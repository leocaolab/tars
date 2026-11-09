use std::path::Path;

use clap::Parser;
use tars_melt::event::call_log::{
    CALL_LOG_SINK, CallDispatch, CallFailure, CallResponse, EvalScoreTable, LlmCallTable,
    RefusalReasons, SentEnd, StreamEnd,
};
use tars_melt::event::log::{LogSink, ParquetLog};
use tars_melt::event::wire::SpawnedProcess;
use tars_types::{OutputLimit, TraceId, ValidationSummary};
use tars_utils::display_chain;
use tokio::runtime::Handle;

use super::*;

const TENANT: &str = "t";
const LIVE_REQUEST: &str = r#"{"prompt":"live-question"}"#;
const LIVE_RESPONSE: &str = r#"{"text":"live-answer"}"#;

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    events: EventsArgs,
}

async fn tars_events(dir: &Path, args: &[&str]) -> String {
    let argv = ["tars", "--store-dir", dir.to_str().unwrap()]
        .into_iter()
        .chain(args.iter().copied());
    run(Cli::parse_from(argv).events).await.unwrap()
}

fn now_ms() -> Millis {
    Millis::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn live_row(tenant: &str, at_ms: Millis) -> LlmCallRow {
    LlmCallRow {
        call_id: CallId::new_v7(at_ms).unwrap(),
        tenant: tenant.parse::<TenantId>().unwrap(),
        session_id: None,
        trace_id: TraceId::new("tr"),
        model: "bound-model".into(),
        request_body: CallBody::Present(LIVE_REQUEST.into()),
        request_sha256: [7; 32],
        has_tools: false,
        has_thinking: false,
        has_structured_output: false,
        temperature: None,
        max_output_tokens: None,
        output_limit: OutputLimit::Unknown,
        dispatch: CallDispatch::Sent {
            degraded: Vec::new(),
            provider_id: None,
            end: SentEnd::Streamed {
                usage: Usage::default(),
                end: StreamEnd::Completed(CallResponse {
                    body: CallBody::Present(LIVE_RESPONSE.into()),
                    actual_model: "actual-model".into(),
                    stop_reason: None,
                }),
            },
        },
        retry_attempts: vec![],
        provider_latency_ms: Some(5),
        pipeline_total_ms: Some(6),
        layers: vec!["telemetry".into()],
        validation_summary: ValidationSummary::default(),
        tags: vec!["dogfood".into()],
    }
}

/// Returns a call a validator rejected, whose writer withheld its request body.
fn rejected_row(tenant: &str, at_ms: Millis) -> LlmCallRow {
    LlmCallRow {
        request_body: CallBody::Withheld,
        dispatch: CallDispatch::Sent {
            degraded: Vec::new(),
            provider_id: None,
            end: SentEnd::FailedBeforeStream(CallFailure::ValidationFailed {
                reason: Some(ValidationReason::NotEmpty {
                    field: "text".into(),
                }),
            }),
        },
        ..live_row(tenant, at_ms)
    }
}

async fn write_calls(dir: &Path, rows: Vec<LlmCallRow>) {
    let sink = ParquetLog::<LlmCallTable>::open(dir).sink(CALL_LOG_SINK, None, &Handle::current());
    for row in rows {
        sink.append(row).await.unwrap();
    }
    sink.close().await.unwrap();
}

async fn write_score(dir: &Path, call_id: CallId) {
    let sink =
        ParquetLog::<EvalScoreTable>::open(dir).sink(CALL_LOG_SINK, None, &Handle::current());
    sink.append(EvalScoreRow {
        score_id: Uuid::new_v4(),
        call_id,
        tenant: TenantId::new(TENANT),
        at_ms: now_ms(),
        evaluator: "live-judge".into(),
        score: 0.75,
        explanation: None,
        tags: vec![],
    })
    .await
    .unwrap();
    sink.close().await.unwrap();
}

/// A store holding, in the call log, a completed call of tenant `t` (scored), an older rejected
/// one of `t`, and one of another tenant.
struct Store {
    dir: tempfile::TempDir,
    live: CallId,
    rejected: CallId,
    foreign: CallId,
}

async fn store() -> Store {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();

    let live = live_row(TENANT, now - 60_000);
    let rejected = rejected_row(TENANT, now - 3_600_000);
    let foreign = live_row("other", now - 60_000);
    let (live_id, rejected_id, foreign_id) = (live.call_id, rejected.call_id, foreign.call_id);
    write_calls(dir.path(), vec![live, rejected, foreign]).await;
    write_score(dir.path(), live_id).await;

    Store {
        dir,
        live: live_id,
        rejected: rejected_id,
        foreign: foreign_id,
    }
}

fn json_lines_of(out: &str) -> Vec<serde_json::Value> {
    out.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn list_shows_the_tenants_calls_from_the_call_log_newest_first() {
    let s = store().await;

    let table = tars_events(s.dir.path(), &["list", "--tenant", TENANT]).await;
    let json =
        json_lines_of(&tars_events(s.dir.path(), &["list", "--tenant", TENANT, "--json"]).await);

    assert_eq!(json.len(), 2, "the tenant's two calls: {json:?}");
    assert_eq!(json[0]["call_id"], s.live.to_string(), "newest first");
    assert_eq!(json[0]["outcome"]["state"], "completed");
    assert_eq!(json[0]["actual_model"], "actual-model");
    assert_eq!(json[1]["call_id"], s.rejected.to_string());
    assert_eq!(
        json[1]["actual_model"],
        serde_json::Value::Null,
        "no response came"
    );
    assert_eq!(json[1]["outcome"]["state"], "failed");
    assert!(!table.contains(&s.foreign.to_string()), "{table}");
    assert!(table.contains(&s.live.to_string()), "{table}");
    let rows: Vec<&str> = table.lines().skip(2).collect();
    assert_eq!(rows.len(), 2, "{table}");
    assert!(rows[0].contains("ok"), "{table}");
    assert!(rows[1].contains("err:validation_failed"), "{table}");
}

#[tokio::test(flavor = "multi_thread")]
async fn show_prints_both_bodies_and_states_a_withheld_body() {
    let s = store().await;

    let live = tars_events(
        s.dir.path(),
        &[
            "show",
            "--tenant",
            TENANT,
            &s.live.to_string(),
            "--with-bodies",
        ],
    )
    .await;
    let withheld = tars_events(
        s.dir.path(),
        &[
            "show",
            "--tenant",
            TENANT,
            &s.rejected.to_string(),
            "--with-bodies",
        ],
    )
    .await;

    assert!(live.contains("live-question"), "{live}");
    assert!(live.contains("live-answer"), "{live}");
    assert!(
        live.contains("\"actual_model\": \"actual-model\""),
        "{live}"
    );
    assert!(live.contains("\"evaluator\": \"live-judge\""), "{live}");

    assert!(withheld.contains("\"state\": \"withheld\""), "{withheld}");
    assert_eq!(withheld.matches("(withheld:").count(), 1, "{withheld}");
    assert!(
        !withheld.contains("live-question"),
        "no body text is made up: {withheld}"
    );
}

async fn shown_session(dir: &Path, id: CallId) -> serde_json::Value {
    let out = tars_events(dir, &["show", "--tenant", TENANT, &id.to_string()]).await;
    serde_json::from_str::<serde_json::Value>(&out).unwrap()["session_id"].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn show_states_a_call_of_no_session_and_shows_its_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let sessionless = live_row(TENANT, now - 60_000);
    let session = tars_types::SessionId::new_v7(1_790_553_600_000).unwrap();
    let in_session = LlmCallRow {
        session_id: Some(session),
        ..live_row(TENANT, now - 30_000)
    };
    let (sessionless_id, in_session_id) = (sessionless.call_id, in_session.call_id);
    write_calls(dir.path(), vec![sessionless, in_session]).await;

    assert_eq!(
        shown_session(dir.path(), sessionless_id).await,
        serde_json::Value::Null
    );
    assert_eq!(
        shown_session(dir.path(), in_session_id).await,
        session.to_string()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn show_gives_a_refused_call_s_reasons_and_null_on_a_served_call() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let refused = LlmCallRow {
        dispatch: CallDispatch::Refused {
            reasons: RefusalReasons::new(vec![tars_types::CompatibilityReason::VisionUnsupported])
                .unwrap(),
        },
        ..live_row(TENANT, now - 10_000)
    };
    let served = live_row(TENANT, now - 20_000);
    let ids = [refused.call_id, served.call_id];
    write_calls(dir.path(), vec![refused, served]).await;

    let mut shown = Vec::new();
    for id in ids {
        let out = tars_events(dir.path(), &["show", "--tenant", TENANT, &id.to_string()]).await;
        shown.push(
            serde_json::from_str::<serde_json::Value>(&out).unwrap()["refusal_reasons"].clone(),
        );
    }
    assert_eq!(
        shown,
        [
            serde_json::json!([{ "kind": "vision" }]),
            serde_json::Value::Null,
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn show_gives_what_the_request_was_sent_without_and_null_on_a_refused_call() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let rows = [
        Some(vec![tars_types::Degradation::ChatTemplateThinkingNotSent]),
        Some(Vec::new()),
        None,
    ]
    .into_iter()
    .zip(1..)
    .map(|(degraded, i)| {
        let dispatch = match degraded {
            Some(degraded) => CallDispatch::Sent {
                degraded,
                provider_id: None,
                end: SentEnd::Cancelled,
            },
            None => CallDispatch::Refused {
                reasons: RefusalReasons::new(vec![
                    tars_types::CompatibilityReason::VisionUnsupported,
                ])
                .unwrap(),
            },
        };
        LlmCallRow {
            dispatch,
            ..live_row(TENANT, now - i * 10_000)
        }
    })
    .collect::<Vec<_>>();
    let ids: Vec<CallId> = rows.iter().map(|r| r.call_id).collect();
    write_calls(dir.path(), rows).await;

    let mut shown = Vec::new();
    for id in ids {
        let out = tars_events(dir.path(), &["show", "--tenant", TENANT, &id.to_string()]).await;
        shown.push(serde_json::from_str::<serde_json::Value>(&out).unwrap()["degraded"].clone());
    }
    assert_eq!(
        shown,
        [
            serde_json::json!(["chat_template_thinking"]),
            serde_json::json!([]),
            serde_json::Value::Null,
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn show_refuses_an_id_that_is_not_a_call_id() {
    let s = store().await;
    let argv = [
        "tars",
        "--store-dir",
        s.dir.path().to_str().unwrap(),
        "show",
        "--tenant",
        TENANT,
    ];
    // A call id is a v7; a v1 is not one.
    let v1 = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

    let err = run(Cli::parse_from(argv.into_iter().chain([v1])).events)
        .await
        .unwrap_err();

    assert!(
        display_chain(&err).contains("version 1"),
        "{}",
        display_chain(&err)
    );
}

/// Copies `from`'s partition files into `into`'s partition of the same day,
/// manifest included: `into`'s directory then lists a file of `from`'s rows.
fn plant_foreign_partition(dir: &Path, from: &str, into: &str, day: tars_types::UtcDay) {
    let part = |t: &str| {
        let tenant = format!("tenant={t}");
        dir.join(tenant)
            .join("llm_call")
            .join(format!("date={day}"))
    };
    let (theirs, ours) = (part(from), part(into));
    std::fs::create_dir_all(&ours).unwrap();
    for entry in std::fs::read_dir(&theirs).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), ours.join(entry.file_name())).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn list_and_show_refuse_a_file_of_another_tenants_rows() {
    let s = store().await;
    let day = s.foreign.day();
    plant_foreign_partition(s.dir.path(), "other", TENANT, day);
    let dir = s.dir.path().to_str().unwrap();
    // `show` reads only the row groups that can hold the id it is given:
    // asked for the foreign call's id, it reads the foreign row.
    let foreign = s.foreign.to_string();

    for args in [
        vec!["list", "--tenant", TENANT],
        vec!["show", "--tenant", TENANT, foreign.as_str()],
    ] {
        let argv = ["tars", "--store-dir", dir].into_iter().chain(args.clone());
        let err = run(Cli::parse_from(argv).events).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("a row of tenant `other` in a partition of tenant `t`"),
            "{args:?}: {err}"
        );
    }
}

/// Overwrites every file in `dir` with bytes no reader can parse.
fn poison(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        std::fs::write(entry.unwrap().path(), b"not parquet, not json").unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn list_and_show_never_open_another_tenants_partitions() {
    let s = store().await;
    let theirs = s
        .dir
        .path()
        .join("tenant=other")
        .join("llm_call")
        .join(format!("date={}", s.foreign.day()));
    poison(&theirs);

    let list = tars_events(s.dir.path(), &["list", "--tenant", TENANT, "--json"]).await;
    let show = tars_events(
        s.dir.path(),
        &["show", "--tenant", TENANT, &s.live.to_string()],
    )
    .await;

    assert_eq!(json_lines_of(&list).len(), 2, "{list}");
    assert!(show.contains(&s.live.to_string()), "{show}");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_with_a_limit_the_newest_day_meets_never_reads_an_older_day() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let old = live_row(TENANT, now - 3 * 86_400_000);
    let newest = [live_row(TENANT, now - 2), live_row(TENANT, now - 1)];
    let old_day = old.call_id.day();
    write_calls(
        dir.path(),
        [old].into_iter().chain(newest.clone()).collect(),
    )
    .await;
    poison(
        &dir.path()
            .join(format!("tenant={TENANT}"))
            .join("llm_call")
            .join(format!("date={old_day}")),
    );

    let out = tars_events(
        dir.path(),
        &[
            "list", "--tenant", TENANT, "--since", "all", "--limit", "2", "--json",
        ],
    )
    .await;

    let ids: Vec<String> = json_lines_of(&out)
        .iter()
        .map(|l| l["call_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        [newest[1].call_id.to_string(), newest[0].call_id.to_string()],
        "{out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_with_a_tag_and_a_limit_the_newest_days_tagged_calls_meet_never_reads_an_older_day() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let tagged = |at_ms: Millis, tag: &str| {
        let mut row = live_row(TENANT, at_ms);
        row.tags = vec![tag.into()];
        row
    };
    let old = tagged(now - 3 * 86_400_000, "cohort");
    let newest = [
        tagged(now - 4, "cohort"),
        tagged(now - 3, "other"),
        tagged(now - 2, "cohort"),
        tagged(now - 1, "other"),
    ];
    let old_day = old.call_id.day();
    write_calls(
        dir.path(),
        [old].into_iter().chain(newest.clone()).collect(),
    )
    .await;
    poison(
        &dir.path()
            .join(format!("tenant={TENANT}"))
            .join("llm_call")
            .join(format!("date={old_day}")),
    );

    let out = tars_events(
        dir.path(),
        &[
            "list", "--tenant", TENANT, "--since", "all", "--tag", "cohort", "--limit", "2",
            "--json",
        ],
    )
    .await;

    let ids: Vec<String> = json_lines_of(&out)
        .iter()
        .map(|l| l["call_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        [newest[2].call_id.to_string(), newest[0].call_id.to_string()],
        "{out}"
    );
}

async fn write_wire(dir: &Path, rows: Vec<WireRow>) {
    let sink = ParquetLog::<wire::WireTable>::open(dir).sink(
        wire::WIRE_LOG_SINK,
        None,
        &Handle::current(),
    );
    for row in rows {
        sink.append(row).await.unwrap();
    }
    sink.close().await.unwrap();
}

fn wire_attempt(call: &LlmCallRow, attempt: u32, at_ms: Millis, outcome: WireOutcome) -> WireRow {
    WireRow {
        tenant: call.tenant.clone(),
        call_id: call.call_id,
        attempt,
        at_ms,
        provider_id: tars_types::ProviderId::new("ant"),
        model: "bound-model".into(),
        transport: Transport::HttpSse { status: Some(200) },
        request_bytes: br#"{"model":"bound-model","messages":[]}"#.to_vec().into(),
        response_bytes: format!("data: attempt-{attempt}-bytes\n\n")
            .into_bytes()
            .into(),
        response_truncated: false,
        outcome,
    }
}

/// The retry past midnight sits in the next day's partition and is still listed.
#[tokio::test(flavor = "multi_thread")]
async fn show_lists_every_wire_attempt_of_a_call_with_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let call = live_row(TENANT, now_ms() - 60_000);
    let next_day = call.at_ms() + 86_400_000;
    let failed = WireRow {
        transport: Transport::HttpSse { status: Some(503) },
        ..wire_attempt(
            &call,
            0,
            call.at_ms(),
            WireOutcome::Failed {
                error: "HTTP 503: upstream overloaded".into(),
            },
        )
    };
    let retried = wire_attempt(&call, 1, next_day, WireOutcome::Completed);
    let id = call.call_id.to_string();
    write_calls(dir.path(), vec![call]).await;
    write_wire(dir.path(), vec![retried, failed]).await;

    let shown: serde_json::Value =
        serde_json::from_str(&tars_events(dir.path(), &["show", "--tenant", TENANT, &id]).await)
            .unwrap();
    let with_bytes = tars_events(
        dir.path(),
        &["show", "--tenant", TENANT, &id, "--with-bodies"],
    )
    .await;

    assert_eq!(shown["wire"]["state"], "recorded");
    let attempts = shown["wire"]["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["attempt"], 0);
    assert_eq!(attempts[0]["outcome"], "failed");
    assert_eq!(attempts[0]["status"], 503);
    assert_eq!(attempts[0]["error"], "HTTP 503: upstream overloaded");
    assert_eq!(attempts[1]["attempt"], 1);
    assert_eq!(attempts[1]["outcome"], "completed");
    assert_eq!(attempts[1]["error"], serde_json::Value::Null);
    assert!(
        with_bytes.contains("=== WIRE ATTEMPT 0 (failed, HTTP 503) REQUEST BYTES ==="),
        "{with_bytes}"
    );
    assert!(with_bytes.contains("data: attempt-0-bytes"), "{with_bytes}");
    assert!(with_bytes.contains("data: attempt-1-bytes"), "{with_bytes}");
}

#[tokio::test(flavor = "multi_thread")]
async fn show_says_so_when_a_call_has_no_wire_record() {
    let dir = tempfile::tempdir().unwrap();
    let call = live_row(TENANT, now_ms() - 60_000);
    let id = call.call_id.to_string();
    write_calls(dir.path(), vec![call]).await;

    let shown: serde_json::Value =
        serde_json::from_str(&tars_events(dir.path(), &["show", "--tenant", TENANT, &id]).await)
            .unwrap();
    let with_bytes = tars_events(
        dir.path(),
        &["show", "--tenant", TENANT, &id, "--with-bodies"],
    )
    .await;

    assert_eq!(shown["wire"], serde_json::json!({ "state": "unrecorded" }));
    assert!(
        with_bytes.contains("(这次调用没有 wire 记录)"),
        "{with_bytes}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn show_prints_a_delegates_stderr_and_counts_none_on_http() {
    let dir = tempfile::tempdir().unwrap();
    let call = live_row(TENANT, now_ms() - 60_000);
    let http = wire_attempt(&call, 0, call.at_ms(), WireOutcome::Completed);
    let delegate = WireRow {
        transport: Transport::Subprocess {
            process: Some(SpawnedProcess {
                argv: vec!["claude".into()],
                stderr: b"You have hit your usage limit\n".to_vec().into(),
            }),
        },
        ..wire_attempt(
            &call,
            1,
            call.at_ms() + 1,
            WireOutcome::Failed {
                error: "claude exited 1".into(),
            },
        )
    };
    let id = call.call_id.to_string();
    write_calls(dir.path(), vec![call]).await;
    write_wire(dir.path(), vec![http, delegate]).await;

    let shown: serde_json::Value =
        serde_json::from_str(&tars_events(dir.path(), &["show", "--tenant", TENANT, &id]).await)
            .unwrap();
    let with_bytes = tars_events(
        dir.path(),
        &["show", "--tenant", TENANT, &id, "--with-bodies"],
    )
    .await;

    let attempts = shown["wire"]["attempts"].as_array().unwrap();
    assert_eq!(attempts[0]["stderr_bytes"], serde_json::Value::Null);
    assert_eq!(attempts[1]["stderr_bytes"], 30);
    assert!(
        with_bytes.contains("=== WIRE ATTEMPT 1 STDERR BYTES ===\nYou have hit your usage limit"),
        "{with_bytes}"
    );
    assert!(
        !with_bytes.contains("WIRE ATTEMPT 0 STDERR"),
        "{with_bytes}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn show_gives_a_delegates_argv_and_none_on_http() {
    let dir = tempfile::tempdir().unwrap();
    let call = live_row(TENANT, now_ms() - 60_000);
    let http = wire_attempt(&call, 0, call.at_ms(), WireOutcome::Completed);
    let delegate = WireRow {
        transport: Transport::Subprocess {
            process: Some(SpawnedProcess {
                argv: vec!["agy".into(), "--prompt".into(), "the prompt".into()],
                stderr: Vec::new().into(),
            }),
        },
        request_bytes: Vec::new().into(),
        ..wire_attempt(&call, 1, call.at_ms() + 1, WireOutcome::Completed)
    };
    let id = call.call_id.to_string();
    write_calls(dir.path(), vec![call]).await;
    write_wire(dir.path(), vec![http, delegate]).await;

    let shown: serde_json::Value =
        serde_json::from_str(&tars_events(dir.path(), &["show", "--tenant", TENANT, &id]).await)
            .unwrap();

    let attempts = shown["wire"]["attempts"].as_array().unwrap();
    assert_eq!(attempts[0]["argv"], serde_json::Value::Null);
    assert_eq!(
        attempts[1]["argv"],
        serde_json::json!(["agy", "--prompt", "the prompt"])
    );
    assert_eq!(attempts[1]["request_bytes"], 0);
}
