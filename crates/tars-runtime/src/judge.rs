//! `Judge` trait + `LlmJudge` reference impl + `run_judge_pass`
//! orchestrator. Implements the offline binary-classification eval
//! phase described in `docs/eval-and-arc-llm-roadmap.md §1.2`.
//!
//! Three pieces:
//!
//! - [`Judge`] — async trait. One method: judge a [`JudgeItem`] and
//!   return a [`JudgeVerdict`]. Implementors choose how (LLM call,
//!   deterministic rule, human-in-the-loop, etc.).
//!
//! - [`LlmJudge`] — reference impl backed by any
//!   `Arc<dyn LlmService>`. Wraps the canonical pipeline (with
//!   cache / retry / telemetry) and parses the response into TP / FP
//!   / Unsure via a prompt template.
//!
//! - [`run_judge_pass`] — top-level orchestrator. Iterates items,
//!   asks the judge for each, aggregates into a [`JudgeReport`].
//!   Caller is responsible for invoking [`ensure_anti_incest`]
//!   first (it's not auto-called — the orchestrator doesn't know
//!   the critic's provider ids).
//!
//! ## Anti-incest
//!
//! Arc's production lesson, re-encoded as a runtime check: a judge
//! whose provider matches the agent-under-judgment's provider has
//! shared blind spots and will systematically rubber-stamp errors
//! the agent itself made. `ensure_anti_incest(judge_id, &critic_provider_ids)`
//! refuses to run when the prefix matches.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use thiserror::Error;

use tars_pipeline::LlmService;
use tars_types::{
    ChatRequest, ChatResponseBuilder, JudgeItem, JudgeReport, JudgeVerdict, JudgedItem, ModelHint,
    ProviderError, RequestContext,
};

/// Async judging trait. One verdict per item.
#[async_trait]
pub trait Judge: Send + Sync {
    /// Stable identifier — typically `"provider:model"`
    /// (e.g. `"anthropic:claude-opus-4-7"`). Used by
    /// [`ensure_anti_incest`] and by [`JudgeReport::judge_id`].
    fn id(&self) -> &str;

    async fn judge(&self, item: &JudgeItem) -> Result<JudgeVerdict, JudgeError>;
}

#[derive(Debug, Error)]
pub enum JudgeError {
    /// Underlying LLM call failed.
    #[error("judge llm error: {0}")]
    Llm(#[from] ProviderError),

    /// Judge response didn't match the expected TP / FP / UNSURE format.
    #[error("judge response could not be parsed: {0}")]
    Parse(String),

    /// Caller tried to run a judge whose provider is the same as the
    /// critic's. See [`ensure_anti_incest`] for the rule.
    #[error("anti-incest violation: judge `{judge}` shares provider with critic `{critic}`")]
    AntiIncest { judge: String, critic: String },
}

/// Refuse-to-run if the judge's provider is the same as any of the
/// critic providers seen in the run being judged. Caller invokes this
/// before [`run_judge_pass`].
///
/// Match policy: case-insensitive prefix on the part before `:` in
/// `judge_id`. So `"anthropic:claude-opus-4-7"` collides with
/// `"anthropic"` but not with `"openai"`.
pub fn ensure_anti_incest(judge_id: &str, critic_provider_ids: &[&str]) -> Result<(), JudgeError> {
    let judge_provider = judge_id.split(':').next().unwrap_or(judge_id);
    for cp in critic_provider_ids {
        if judge_provider.eq_ignore_ascii_case(cp) {
            return Err(JudgeError::AntiIncest {
                judge: judge_provider.to_string(),
                critic: cp.to_string(),
            });
        }
    }
    Ok(())
}

/// Iterate `items`, ask `judge` for each, return a [`JudgeReport`]
/// with aggregates + per-item verdicts.
///
/// **Note**: this is sequential. Parallel batching is intentionally
/// not in V1 — judges run against rate-limited APIs and the simplest
/// thing that works is one request at a time. A parallel variant
/// (or just letting the caller spawn tasks) is a V2 concern.
pub async fn run_judge_pass(
    items: Vec<JudgeItem>,
    judge: &dyn Judge,
) -> Result<JudgeReport, JudgeError> {
    let mut verdicts = Vec::with_capacity(items.len());
    let mut tp: u32 = 0;
    let mut fp: u32 = 0;
    let mut un: u32 = 0;
    for item in &items {
        let v = judge.judge(item).await?;
        match &v {
            JudgeVerdict::TruePositive => tp = tp.saturating_add(1),
            JudgeVerdict::FalsePositive => fp = fp.saturating_add(1),
            JudgeVerdict::Unsure { .. } => un = un.saturating_add(1),
        }
        verdicts.push(JudgedItem {
            item_id: item.item_id.clone(),
            verdict: v,
        });
    }
    Ok(JudgeReport {
        judge_id: judge.id().to_string(),
        item_count: items.len().try_into().unwrap_or(u32::MAX),
        true_positives: tp,
        false_positives: fp,
        unsure: un,
        verdicts,
    })
}

// ─── LlmJudge — reference impl ─────────────────────────────────────

/// The default prompt for binary TP/FP/Unsure judging. Override via
/// [`LlmJudge::with_prompt_template`] for domain-specific judging.
///
/// Placeholders: `{input}` `{output}` `{expected}` `{context}`.
/// Missing optional fields substitute empty strings.
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

/// LLM-backed judge. Thin wrapper around an `Arc<dyn LlmService>`
/// (the canonical pipeline you'd use anywhere else in tars).
pub struct LlmJudge {
    service: Arc<dyn LlmService>,
    id: String,
    model: ModelHint,
    prompt_template: String,
    /// Request context threaded into every `service.call`. The
    /// `Judge::judge` trait method takes no context (it's
    /// provider-agnostic), so the LLM-backed impl carries its own —
    /// set it via [`Self::with_request_context`] in production to
    /// supply real IAM / trace / deadline. Defaults to
    /// `RequestContext::test_default()` for tests / dev.
    ctx: RequestContext,
}

impl LlmJudge {
    /// `id` is the judge identifier (typically `"provider:model"`).
    /// `service` is a pipeline-wrapped service for the judge model.
    pub fn new(service: Arc<dyn LlmService>, id: impl Into<String>, model: ModelHint) -> Self {
        Self {
            service,
            id: id.into(),
            model,
            prompt_template: DEFAULT_JUDGE_PROMPT.to_string(),
            ctx: RequestContext::test_default(),
        }
    }

    pub fn with_prompt_template(mut self, t: impl Into<String>) -> Self {
        self.prompt_template = t.into();
        self
    }

    /// Supply the production [`RequestContext`] (trace / tenant /
    /// principal / deadline / cancel) used for every judge LLM call.
    /// Without this, the judge runs with `RequestContext::test_default()`.
    pub fn with_request_context(mut self, ctx: RequestContext) -> Self {
        self.ctx = ctx;
        self
    }
}

#[async_trait]
impl Judge for LlmJudge {
    fn id(&self) -> &str {
        &self.id
    }

    async fn judge(&self, item: &JudgeItem) -> Result<JudgeVerdict, JudgeError> {
        let prompt = self
            .prompt_template
            .replace("{input}", &item.input)
            .replace("{output}", &item.output)
            .replace("{expected}", item.expected.as_deref().unwrap_or(""))
            .replace("{context}", item.context.as_deref().unwrap_or(""));

        let req = ChatRequest::user(self.model.clone(), prompt);
        let stream = self.service.clone().call(req, self.ctx.clone()).await?;
        let mut s = stream;
        let mut acc = ChatResponseBuilder::new();
        // Bound the stream so a runaway / malicious model can't OOM the
        // judge: the verdict only needs the first line (TP/FP/UNSURE),
        // so a few thousand events is already pathological. The
        // deadline/cancel side of DoS is handled upstream via
        // `self.ctx` (deadline + cancel threaded into the pipeline);
        // this cap covers the unbounded-size axis.
        const MAX_STREAM_EVENTS: usize = 10_000;
        let mut seen = 0usize;
        while let Some(event) = s.next().await {
            acc.apply(event?);
            seen += 1;
            if seen > MAX_STREAM_EVENTS {
                return Err(JudgeError::Parse(format!(
                    "judge stream exceeded {MAX_STREAM_EVENTS} events without completing; \
                     aborting to bound memory"
                )));
            }
        }
        let resp = acc.finish();
        parse_verdict(&resp.text)
    }
}

/// Parse a free-text judge response into a [`JudgeVerdict`].
///
/// Accepts a forgiving range of formats — any line starting with
/// `TP`, `FP`, or `UNSURE` (case-insensitive); also accepts `true
/// positive` / `false positive` phrases. Anything else returns
/// [`JudgeError::Parse`] so the caller can decide retry / escalate.
fn parse_verdict(text: &str) -> Result<JudgeVerdict, JudgeError> {
    // Look at the first non-empty line — the prompt asks for the
    // verdict on the first line.
    let first_line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let upper = first_line.to_uppercase();

    if upper.starts_with("TP") || upper.contains("TRUE POSITIVE") {
        Ok(JudgeVerdict::TruePositive)
    } else if upper.starts_with("FP") || upper.contains("FALSE POSITIVE") {
        Ok(JudgeVerdict::FalsePositive)
    } else if upper.starts_with("UNSURE") {
        // Extract the reason if the model put it after `UNSURE:` or
        // `UNSURE — ...`.
        let reason = first_line
            .trim_start_matches(|c: char| c.is_ascii_alphabetic())
            .trim_start_matches(|c: char| c == ':' || c == '—' || c == '-' || c.is_whitespace())
            .trim()
            .to_string();
        Ok(JudgeVerdict::Unsure { reason })
    } else {
        Err(JudgeError::Parse(format!(
            "unrecognized verdict in response: {first_line:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mock judge that returns a pre-canned verdict per item_id, by
    /// position. Useful for orchestrator tests without standing up
    /// an LLM service.
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
            self.verdicts
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| JudgeError::Parse("scripted judge ran out of verdicts".into()))
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
                JudgeVerdict::TruePositive,
                JudgeVerdict::TruePositive,
                JudgeVerdict::FalsePositive,
                JudgeVerdict::Unsure {
                    reason: "ambig".into(),
                },
            ],
        );
        let report = run_judge_pass(vec![item("a"), item("b"), item("c"), item("d")], &judge)
            .await
            .unwrap();
        assert_eq!(report.judge_id, "openai:gpt-4o");
        assert_eq!(report.item_count, 4);
        assert_eq!(report.true_positives, 2);
        assert_eq!(report.false_positives, 1);
        assert_eq!(report.unsure, 1);
        assert_eq!(report.verdicts.len(), 4);
        // Per-item ordering preserved.
        assert_eq!(report.verdicts[0].item_id, "a");
        assert!(matches!(
            report.verdicts[0].verdict,
            JudgeVerdict::TruePositive
        ));
        // 2 / (2+1) = 0.666…
        assert!((report.precision().unwrap() - 2.0 / 3.0).abs() < 1e-9);
        // 1 / 4 = 0.25
        assert!((report.unsure_rate().unwrap() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn anti_incest_blocks_same_provider() {
        let err = ensure_anti_incest("anthropic:claude-opus-4-7", &["anthropic"])
            .expect_err("must reject");
        assert!(matches!(err, JudgeError::AntiIncest { .. }));
    }

    #[test]
    fn anti_incest_case_insensitive() {
        let err = ensure_anti_incest("Anthropic:opus", &["anthropic"]);
        assert!(err.is_err());
    }

    #[test]
    fn anti_incest_passes_different_provider() {
        ensure_anti_incest("openai:gpt-4o", &["anthropic", "vllm_local"]).unwrap();
    }

    #[test]
    fn anti_incest_works_without_colon_in_id() {
        // `judge_id = "openai"` (no `:model` suffix) still works.
        ensure_anti_incest("openai", &["anthropic"]).unwrap();
        let err = ensure_anti_incest("anthropic", &["anthropic"]);
        assert!(err.is_err());
    }

    #[test]
    fn parse_verdict_tp_variants() {
        assert!(matches!(
            parse_verdict("TP").unwrap(),
            JudgeVerdict::TruePositive
        ));
        assert!(matches!(
            parse_verdict("tp\n(some explanation)").unwrap(),
            JudgeVerdict::TruePositive
        ));
        assert!(matches!(
            parse_verdict("Verdict: true positive").unwrap(),
            JudgeVerdict::TruePositive
        ));
    }

    #[test]
    fn parse_verdict_fp_variants() {
        assert!(matches!(
            parse_verdict("FP").unwrap(),
            JudgeVerdict::FalsePositive
        ));
        assert!(matches!(
            parse_verdict("fp: bogus").unwrap(),
            JudgeVerdict::FalsePositive
        ));
        assert!(matches!(
            parse_verdict("This is a false positive").unwrap(),
            JudgeVerdict::FalsePositive
        ));
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
        assert!(matches!(err, JudgeError::Parse(_)));
    }

    #[test]
    fn parse_verdict_skips_blank_leading_lines() {
        let v = parse_verdict("\n\n  TP").unwrap();
        assert!(matches!(v, JudgeVerdict::TruePositive));
    }
}
