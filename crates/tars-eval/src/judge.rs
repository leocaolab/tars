//! Grades outputs offline as correct, wrong or unsure, one verdict per item, and collects the
//! verdicts into a [`JudgeReport`].
//!
//! ## Self-judging
//!
//! A judge on the same provider as the agent it judges shares its blind spots and rubber-stamps
//! the agent's own errors, so [`check_providers`] refuses that pairing, and
//! [`check_vendors`] refuses a judge whose model comes from the same vendor as the run's even
//! through another provider (`claude_cli` judging an `anthropic` run). A model whose vendor the
//! catalog does not know is let through, and the check says it could not verify.

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tars_config::Vendor;
use thiserror::Error;

use tars_llm::{LlmError, LlmService, ProviderError};
use tars_types::{Caller, ChatRequest, ChatResponseBuilder, ProviderId};

use crate::judge_stats::{
    JudgeFailure, JudgeItem, JudgeReport, JudgeVerdict, JudgedItem, UnjudgedItem,
};

/// Gives one verdict per item.
#[async_trait]
pub trait Judge: Send + Sync {
    /// Returns the judge's stable name, typically `"provider:model"`, which becomes
    /// [`JudgeReport::judge_id`].
    fn id(&self) -> &str;

    async fn judge(&self, item: &JudgeItem) -> Result<JudgeVerdict, JudgeError>;
}

#[derive(Debug, Error)]
pub enum JudgeError {
    #[error("judge llm error")]
    Llm(#[from] LlmError),

    /// The reply's first line does not open with `TP`, `FP` or `UNSURE`; `first_line` is that
    /// line as the judge wrote it.
    #[error("the judge's first line is not a verdict: {first_line:?}")]
    UnrecognizedVerdict { first_line: String },

    /// The arg-judge reply's first line does not open with a yes or a no; `first_line` is that
    /// line as the judge wrote it.
    #[error("the arg-judge's first line is neither a yes nor a no: {first_line:?}")]
    UnrecognizedEquivalence { first_line: String },

    /// The reply ran past `limit` stream events without completing, and was abandoned so a
    /// runaway model cannot exhaust memory.
    #[error("judge stream exceeded {limit} events without completing")]
    StreamTooLong { limit: usize },
}

impl JudgeError {
    /// Returns the failure as a report records it: the LLM error's kind and its message cut to
    /// 500 chars, or the reply's first line.
    pub fn failure(&self) -> JudgeFailure {
        match self {
            Self::Llm(e) => JudgeFailure::Call {
                kind: e.kind(),
                message: crate::truncate(&tars_utils::display_chain(e), 500),
            },
            Self::UnrecognizedVerdict { first_line }
            | Self::UnrecognizedEquivalence { first_line } => JudgeFailure::UnrecognizedReply {
                first_line: first_line.clone(),
            },
            Self::StreamTooLong { limit } => JudgeFailure::StreamTooLong { limit: *limit },
        }
    }
}

/// A provider error mid-stream is the same LLM failure as one the call returns.
impl From<ProviderError> for JudgeError {
    fn from(e: ProviderError) -> Self {
        Self::Llm(LlmError::Provider(e))
    }
}

/// A judge refused because it is the provider that produced the run it would judge.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("the judge is `{provider}`, the provider that produced the run")]
pub struct SharedProvider {
    pub provider: ProviderId,
}

/// Refuses a judge that is the run's own provider, compared without case (`Anthropic` is
/// `anthropic`); the caller runs it before [`run_judge_pass`].
///
/// # Errors
///
/// [`SharedProvider`] when the two ids name one provider.
pub fn check_providers(run: &ProviderId, judge: &ProviderId) -> Result<(), SharedProvider> {
    if judge.as_str().eq_ignore_ascii_case(run.as_str()) {
        Err(SharedProvider {
            provider: run.clone(),
        })
    } else {
        Ok(())
    }
}

/// What is known, of a judge let through, about whether its model's vendor differs from the
/// run's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "vendors", rename_all = "snake_case")]
pub enum VendorCheck {
    Distinct {
        run: Vendor,
        judge: Vendor,
    },
    /// The run's model has no known vendor, or the run predates recording it, so the judge may
    /// share it.
    UnknownRunVendor {
        judge: Option<Vendor>,
    },
    /// The judge's model has no known vendor, so it may be the run's.
    UnknownJudgeVendor {
        run: Vendor,
    },
}

impl VendorCheck {
    /// Returns whether both vendors were known, so the judge is known to be another vendor's.
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Distinct { .. })
    }
}

/// A judge refused because its model and the run's come from one vendor.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("the judge's model and the run's are both {vendor}'s")]
pub struct SharedVendor {
    pub vendor: Vendor,
}

/// Decides whether a judge may grade a run from the vendors of their models: refused when both
/// are known and equal, let through otherwise, saying whether that was verified.
///
/// # Errors
///
/// [`SharedVendor`] when both vendors are known and the same.
pub fn check_vendors(
    run: Option<Vendor>,
    judge: Option<Vendor>,
) -> Result<VendorCheck, SharedVendor> {
    match (run, judge) {
        (Some(run), Some(judge)) if run == judge => Err(SharedVendor { vendor: run }),
        (Some(run), Some(judge)) => Ok(VendorCheck::Distinct { run, judge }),
        (None, judge) => Ok(VendorCheck::UnknownRunVendor { judge }),
        (Some(run), None) => Ok(VendorCheck::UnknownJudgeVendor { run }),
    }
}

/// Judges every item in order, one call at a time, and returns the verdicts with the items the
/// judge failed on. Sequential because judges run against rate-limited APIs. A failure on one
/// item is recorded against it and the pass goes on: it says nothing about the other items.
pub async fn run_judge_pass(items: Vec<JudgeItem>, judge: &dyn Judge) -> JudgeReport {
    let mut verdicts = Vec::with_capacity(items.len());
    let mut unjudged = Vec::new();
    for item in items {
        match judge.judge(&item).await {
            Ok(verdict) => verdicts.push(JudgedItem {
                item_id: item.item_id,
                verdict,
            }),
            Err(e) => unjudged.push(UnjudgedItem {
                item_id: item.item_id,
                failure: e.failure(),
            }),
        }
    }
    JudgeReport {
        judge_id: judge.id().to_string(),
        verdicts,
        unjudged,
    }
}

// ─── LlmJudge — reference impl ─────────────────────────────────────

/// The default prompt for TP/FP/Unsure judging, replaced with [`LlmJudge::with_prompt_template`].
///
/// Placeholders: `{input}` `{output}` `{expected}` `{context}`; an absent optional field becomes
/// an empty string.
pub const DEFAULT_JUDGE_PROMPT: &str = "\
You are an impartial judge evaluating whether an agent's output is correct.

The agent was given this input:
--- INPUT ---
{input}

The agent produced this output:
--- OUTPUT ---
{output}

Expected output (when known):
--- EXPECTED ---
{expected}

Additional context:
--- CONTEXT ---
{context}

Reply on the FIRST LINE with exactly one of:
  TP        — output is correct
  FP        — output is wrong
  UNSURE    — cannot decide

If UNSURE, put a one-line reason on the same line after `UNSURE: `.

Do not add any other content before the verdict line.
";

/// A judge that asks a model through an [`LlmService`].
pub struct LlmJudge {
    service: LlmService,
    id: String,
    prompt_template: String,
    /// The caller the judge's model calls are made for; `Judge::judge` takes no context, so it is
    /// given once, here.
    caller: Caller,
}

impl LlmJudge {
    /// Builds a judge over `service` with the default prompt, named `id` (typically
    /// `"provider:model"`), whose every call is made for `caller`.
    pub fn new(service: LlmService, id: impl Into<String>, caller: Caller) -> Self {
        Self {
            service,
            id: id.into(),
            prompt_template: DEFAULT_JUDGE_PROMPT.to_string(),
            caller,
        }
    }

    pub fn with_prompt_template(mut self, t: impl Into<String>) -> Self {
        self.prompt_template = t.into();
        self
    }
}

#[async_trait]
impl Judge for LlmJudge {
    fn id(&self) -> &str {
        &self.id
    }

    async fn judge(&self, item: &JudgeItem) -> Result<JudgeVerdict, JudgeError> {
        let req = ChatRequest::user(render_prompt(&self.prompt_template, item));
        let text = reply_text(&self.service, req, self.caller.for_call()).await?;
        parse_verdict(&text)
    }
}

/// The most stream events a judge reply may take; the verdict is on its first line, so a reply
/// past it is a runaway model, abandoned before it exhausts memory.
const MAX_STREAM_EVENTS: usize = 10_000;

/// Returns the text of a judge's reply to `req`. Time is bounded only by the deadline `ctx`
/// carries.
///
/// # Errors
///
/// Fails when the call or its stream fails, and when the reply runs past [`MAX_STREAM_EVENTS`].
pub(crate) async fn reply_text(
    service: &LlmService,
    req: ChatRequest,
    ctx: Caller,
) -> Result<String, JudgeError> {
    let mut stream = service.clone().call(req, ctx).await.result?;
    let mut acc = ChatResponseBuilder::new();
    let mut seen = 0usize;
    while let Some(event) = stream.next().await {
        acc.apply(event?);
        seen += 1;
        if seen > MAX_STREAM_EVENTS {
            return Err(JudgeError::StreamTooLong {
                limit: MAX_STREAM_EVENTS,
            });
        }
    }
    Ok(acc.finish().text)
}

/// Fills each placeholder of `template` with the item's field; see [`fill_template`].
fn render_prompt(template: &str, item: &JudgeItem) -> String {
    fill_template(
        template,
        &[
            ("{input}", &item.input),
            ("{output}", &item.output),
            ("{expected}", item.expected.as_deref().unwrap_or("")),
            ("{context}", item.context.as_deref().unwrap_or("")),
        ],
    )
}

/// Replaces each placeholder of `template` with its value, scanning the template once: a
/// placeholder spelled inside an inserted value (a Python f-string quoting `{expected}`) stays as
/// the value's text, so one field cannot leak into another section.
pub(crate) fn fill_template(template: &str, fields: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let at = &rest[open..];
        match fields.iter().find(|(name, _)| at.starts_with(name)) {
            Some((name, value)) => {
                out.push_str(value);
                rest = &at[name.len()..];
            }
            None => {
                out.push('{');
                rest = &at[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Returns a reply's first non-empty line, trimmed; empty when the reply has none.
pub(crate) fn first_line(text: &str) -> &str {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

/// Splits a line into its first word, without markdown emphasis or trailing punctuation, and the
/// rest of the line: `**TP**: fine` is `("TP", "fine")`.
pub(crate) fn first_word(line: &str) -> (&str, &str) {
    let is_markup = |c: char| matches!(c, '*' | '_' | '`' | '#' | '>');
    let line = line.trim_start_matches(|c: char| is_markup(c) || c.is_whitespace());
    let end = line
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(line.len());
    let (word, rest) = line.split_at(end);
    let rest = rest.trim_start_matches(|c: char| {
        is_markup(c) || c.is_whitespace() || matches!(c, ':' | '—' | '-' | ',' | '.')
    });
    (word, rest.trim_end())
}

/// Reads a [`JudgeVerdict`] from the first word of the reply's first non-empty line, ignoring
/// case and markdown: `TP`, `FP` or `UNSURE`. A verdict said in words is not guessed at, since
/// "not a false positive" names the phrase it denies.
///
/// # Errors
///
/// Returns [`JudgeError::UnrecognizedVerdict`] for any other first word, so the caller decides
/// whether to retry.
fn parse_verdict(text: &str) -> Result<JudgeVerdict, JudgeError> {
    let line = first_line(text);
    let (word, rest) = first_word(line);
    match word.to_ascii_uppercase().as_str() {
        "TP" => Ok(JudgeVerdict::Correct),
        "FP" => Ok(JudgeVerdict::Wrong),
        "UNSURE" => Ok(JudgeVerdict::Unsure {
            reason: rest.to_string(),
        }),
        _ => Err(JudgeError::UnrecognizedVerdict {
            first_line: line.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A judge that returns its scripted verdicts in order.
    struct ScriptedJudge {
        id: String,
        verdicts: Mutex<std::collections::VecDeque<JudgeVerdict>>,
    }

    impl ScriptedJudge {
        fn new(id: &str, vs: Vec<JudgeVerdict>) -> Self {
            Self {
                id: id.into(),
                verdicts: Mutex::new(vs.into()),
            }
        }
    }

    #[async_trait]
    impl Judge for ScriptedJudge {
        fn id(&self) -> &str {
            &self.id
        }

        async fn judge(&self, _item: &JudgeItem) -> Result<JudgeVerdict, JudgeError> {
            self.verdicts.lock().unwrap().pop_front().ok_or_else(|| {
                JudgeError::UnrecognizedVerdict {
                    first_line: "scripted judge ran out of verdicts".into(),
                }
            })
        }
    }

    fn item(id: &str) -> JudgeItem {
        JudgeItem {
            item_id: id.into(),
            input: "in".into(),
            output: "out".into(),
            expected: None,
            context: None,
        }
    }

    #[tokio::test]
    async fn run_judge_pass_aggregates_tp_fp_unsure() {
        let judge = ScriptedJudge::new(
            "openai:gpt-4o",
            vec![
                JudgeVerdict::Correct,
                JudgeVerdict::Correct,
                JudgeVerdict::Wrong,
                JudgeVerdict::Unsure {
                    reason: "ambig".into(),
                },
            ],
        );
        let report = run_judge_pass(vec![item("a"), item("b"), item("c"), item("d")], &judge).await;
        assert_eq!(report.judge_id, "openai:gpt-4o");
        assert_eq!(report.item_count(), 4);
        assert_eq!(report.correct(), 2);
        assert_eq!(report.wrong(), 1);
        assert_eq!(report.unsure(), 1);
        assert_eq!(report.verdicts.len(), 4);
        assert_eq!(report.verdicts[0].item_id, "a");
        assert!(matches!(report.verdicts[0].verdict, JudgeVerdict::Correct));
        // 2 / (2+1) = 0.666…
        assert!((report.precision().unwrap() - 2.0 / 3.0).abs() < 1e-9);
        // 1 / 4 = 0.25
        assert!((report.unsure_rate().unwrap() - 0.25).abs() < 1e-9);
    }

    #[tokio::test]
    async fn an_item_the_judge_fails_on_is_recorded_and_the_pass_goes_on() {
        let judge = ScriptedJudge::new("j", vec![JudgeVerdict::Correct]);
        let report = run_judge_pass(vec![item("a"), item("b")], &judge).await;
        assert_eq!(report.item_count(), 2);
        assert_eq!(report.correct(), 1);
        assert_eq!(
            report.unjudged,
            [UnjudgedItem {
                item_id: "b".into(),
                failure: JudgeFailure::UnrecognizedReply {
                    first_line: "scripted judge ran out of verdicts".into()
                }
            }]
        );
        assert_eq!(report.precision(), Some(1.0));
    }

    fn vendor(name: &str) -> Vendor {
        Vendor::from(name.to_string())
    }

    #[test]
    fn a_judge_of_the_runs_vendor_is_refused() {
        let err = check_vendors(Some(vendor("anthropic")), Some(vendor("anthropic"))).unwrap_err();
        assert_eq!(err.vendor, vendor("anthropic"));
    }

    #[test]
    fn a_judge_of_another_vendor_is_verified() {
        let check = check_vendors(Some(vendor("anthropic")), Some(vendor("google"))).unwrap();
        assert!(check.is_verified());
    }

    #[test]
    fn an_unknown_run_vendor_lets_the_judge_through_unverified() {
        let check = check_vendors(None, Some(vendor("anthropic"))).unwrap();
        assert_eq!(
            check,
            VendorCheck::UnknownRunVendor {
                judge: Some(vendor("anthropic"))
            }
        );
        assert!(!check.is_verified());
    }

    #[test]
    fn an_unknown_judge_vendor_lets_the_judge_through_unverified() {
        let check = check_vendors(Some(vendor("anthropic")), None).unwrap();
        assert!(!check.is_verified());
    }

    #[test]
    fn neither_vendor_known_is_unverified_not_refused() {
        let check = check_vendors(None, None).unwrap();
        assert_eq!(check, VendorCheck::UnknownRunVendor { judge: None });
    }

    fn provider(id: &str) -> ProviderId {
        id.parse::<ProviderId>().unwrap()
    }

    #[test]
    fn a_judge_that_is_the_runs_provider_is_refused() {
        let err = check_providers(&provider("anthropic"), &provider("anthropic"))
            .expect_err("must reject");
        assert_eq!(err.provider, provider("anthropic"));
    }

    #[test]
    fn providers_are_compared_without_case() {
        assert!(check_providers(&provider("anthropic"), &provider("Anthropic")).is_err());
    }

    #[test]
    fn a_judge_on_another_provider_is_let_through() {
        check_providers(&provider("anthropic"), &provider("openai")).unwrap();
    }

    #[test]
    fn a_verdict_word_wrapped_in_markdown_is_read() {
        assert_eq!(parse_verdict("**TP**").unwrap(), JudgeVerdict::Correct);
        assert_eq!(parse_verdict("`FP`: wrong").unwrap(), JudgeVerdict::Wrong);
    }

    #[test]
    fn the_first_word_decides_over_a_phrase_later_in_the_line() {
        assert_eq!(
            parse_verdict("UNSURE: might be a true positive").unwrap(),
            JudgeVerdict::Unsure {
                reason: "might be a true positive".into()
            }
        );
        assert_eq!(
            parse_verdict("FP — not a true positive").unwrap(),
            JudgeVerdict::Wrong
        );
    }

    #[test]
    fn a_line_naming_both_phrases_is_not_a_verdict() {
        let err = parse_verdict("neither a true positive nor a false positive").unwrap_err();
        assert!(
            matches!(err, JudgeError::UnrecognizedVerdict { .. }),
            "{err}"
        );
    }

    #[test]
    fn an_unrecognized_verdict_carries_the_line_as_written() {
        match parse_verdict("\n  Looks Fine to me  \nTP") {
            Err(JudgeError::UnrecognizedVerdict { first_line }) => {
                assert_eq!(first_line, "Looks Fine to me")
            }
            other => panic!("expected an unrecognized verdict, got {other:?}"),
        }
    }

    #[test]
    fn a_placeholder_inside_a_field_is_the_fields_text() {
        let item = JudgeItem {
            item_id: "c".into(),
            input: "f\"{expected}\" {output}".into(),
            output: "{input}".into(),
            expected: Some("SECRET".into()),
            context: Some("{context}".into()),
        };
        assert_eq!(
            render_prompt("I={input} O={output} E={expected} C={context}", &item),
            "I=f\"{expected}\" {output} O={input} E=SECRET C={context}"
        );
    }

    #[test]
    fn a_brace_that_opens_no_placeholder_is_kept() {
        assert_eq!(
            render_prompt("{ {inpu} {input}} {", &item("x")),
            "{ {inpu} in} {"
        );
    }

    #[test]
    fn parse_verdict_tp_variants() {
        assert!(matches!(
            parse_verdict("TP").unwrap(),
            JudgeVerdict::Correct
        ));
        assert!(matches!(
            parse_verdict("tp\n(some explanation)").unwrap(),
            JudgeVerdict::Correct
        ));
    }

    #[test]
    fn parse_verdict_fp_variants() {
        assert!(matches!(parse_verdict("FP").unwrap(), JudgeVerdict::Wrong));
        assert!(matches!(
            parse_verdict("fp: bogus").unwrap(),
            JudgeVerdict::Wrong
        ));
    }

    #[test]
    fn a_verdict_said_in_words_is_not_guessed() {
        for reply in [
            "Verdict: true positive",
            "This is a false positive",
            "This is not a false positive",
        ] {
            assert!(
                matches!(
                    parse_verdict(reply),
                    Err(JudgeError::UnrecognizedVerdict { ref first_line }) if first_line == reply
                ),
                "{reply}"
            );
        }
    }

    #[test]
    fn parse_verdict_unsure_extracts_reason() {
        let v = parse_verdict("UNSURE: not enough context").unwrap();
        match v {
            JudgeVerdict::Unsure { reason } => assert_eq!(reason, "not enough context"),
            other => panic!("expected Unsure, got {other:?}"),
        }
    }

    #[test]
    fn parse_verdict_unknown_errors() {
        let err = parse_verdict("I think the agent did a great job!").expect_err("must error");
        assert!(matches!(err, JudgeError::UnrecognizedVerdict { .. }));
    }

    #[test]
    fn parse_verdict_skips_blank_leading_lines() {
        let v = parse_verdict("\n\n  TP").unwrap();
        assert!(matches!(v, JudgeVerdict::Correct));
    }

    #[tokio::test]
    async fn llm_judge_calls_are_made_for_the_tenant_its_caller_names() {
        use tars_types::{TenantId, TraceId};

        let recorder = crate::test_support::CallRecorder::new("judge_rec", "TP");
        let service = LlmService::of(recorder.clone(), "m");
        let caller = Caller::new(TenantId::new("judge-tenant"), TraceId::new("t"));
        let judge = LlmJudge::new(service, "judge_rec:m", caller);

        judge.judge(&item("i1")).await.unwrap();

        assert_eq!(recorder.tenants(), vec![TenantId::new("judge-tenant")]);
    }
}
