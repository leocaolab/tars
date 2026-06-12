//! LLM-judged tool-argument equivalence — Doc 26 M3' part 2.
//!
//! The deterministic `MatchMode::Args` scorer compares arguments byte-for-byte:
//! `search("ducks")` ≠ `search("duck")`. Sometimes that's too strict — two arg
//! sets can be *semantically* equivalent (same intent / same tool behavior)
//! without being byte-identical. This module adds an **async** judge that an
//! eval can opt into for the `args` dimension.
//!
//! Design: the scorer in [`crate::trajectory_match`] stays a **pure function**.
//! The LLM call lives here, at the async call layer — [`args_match_judged`]
//! does the same exact name-sequence alignment as `MatchMode::Args`, then asks
//! the judge only about the arg pairs that *differ* byte-wise. Verdicts are
//! cached (symmetric in the pair) so a repeated `(tool, a, b)` costs one call;
//! byte-equal args short-circuit with no call at all.
//!
//! Anti-incest: reuse [`crate::judge::ensure_anti_incest`] — the judge model
//! must not be the same provider that produced the trajectory under test.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use serde_json::Value;

use tars_pipeline::LlmService;
use tars_types::{ChatRequest, ChatResponseBuilder, ModelHint, RequestContext};

use crate::judge::JudgeError;
use crate::trajectory_match::ToolStep;

const ARG_JUDGE_PROMPT: &str = "\
You are checking whether two argument sets for the tool `{tool}` are \
SEMANTICALLY EQUIVALENT — would they drive the same tool behaviour / intent, \
even if not byte-identical?\n\n\
Arguments A: {a}\n\
Arguments B: {b}\n\n\
Answer on the FIRST line with exactly `YES` (equivalent) or `NO` (not \
equivalent). You may add a brief reason on later lines.";

/// An LLM that decides whether two tool-argument sets are semantically equal.
pub struct ArgEquivalenceJudge {
    service: Arc<dyn LlmService>,
    id: String,
    model: ModelHint,
    ctx: RequestContext,
    /// `(tool, lo, hi) -> equivalent?`, where `lo`/`hi` are the canonical-JSON
    /// strings of the two arg sets, ordered so `(a,b)` and `(b,a)` share a slot.
    cache: Mutex<HashMap<(String, String, String), bool>>,
}

impl ArgEquivalenceJudge {
    /// `id` is the judge identifier (typically `"provider:model"`), checked by
    /// [`crate::judge::ensure_anti_incest`]. `service` is a pipeline-wrapped
    /// service for the judge model.
    pub fn new(service: Arc<dyn LlmService>, id: impl Into<String>, model: ModelHint) -> Self {
        Self {
            service,
            id: id.into(),
            model,
            ctx: RequestContext::test_default(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Supply the production [`RequestContext`] (trace / tenant / deadline /
    /// cancel) used for every judge call.
    pub fn with_request_context(mut self, ctx: RequestContext) -> Self {
        self.ctx = ctx;
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Are `a` and `b` semantically equivalent arguments for `tool`?
    /// Byte-equal short-circuits to `true` with no LLM call; otherwise the
    /// verdict is judged once and cached.
    pub async fn args_equivalent(
        &self,
        tool: &str,
        a: &Value,
        b: &Value,
    ) -> Result<bool, JudgeError> {
        if a == b {
            return Ok(true);
        }
        let ca = canonical(a);
        let cb = canonical(b);
        // Symmetric key: equivalence is order-independent.
        let key = if ca <= cb {
            (tool.to_string(), ca.clone(), cb.clone())
        } else {
            (tool.to_string(), cb.clone(), ca.clone())
        };
        if let Some(v) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key).copied() {
            return Ok(v);
        }

        let prompt = ARG_JUDGE_PROMPT
            .replace("{tool}", tool)
            .replace("{a}", &ca)
            .replace("{b}", &cb);
        let req = ChatRequest::user(self.model.clone(), prompt);
        let mut stream = self.service.clone().call(req, self.ctx.clone()).await?;
        let mut acc = ChatResponseBuilder::new();
        // Bound the stream so a runaway model can't OOM the judge (mirrors
        // LlmJudge); the verdict is on the first line.
        const MAX_STREAM_EVENTS: usize = 10_000;
        let mut seen = 0usize;
        while let Some(event) = stream.next().await {
            acc.apply(event?);
            seen += 1;
            if seen > MAX_STREAM_EVENTS {
                return Err(JudgeError::Parse(format!(
                    "arg-judge stream exceeded {MAX_STREAM_EVENTS} events; aborting"
                )));
            }
        }
        let verdict = parse_yes_no(&acc.finish().text)?;
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, verdict);
        Ok(verdict)
    }
}

/// Score `actual` vs `expected` like `MatchMode::Args` (strict name sequence),
/// but treat byte-different arguments as a match when the judge calls them
/// equivalent. All-or-nothing: returns `1.0` iff names align AND every arg
/// pair is equal-or-judged-equivalent, else `0.0`.
pub async fn args_match_judged(
    actual: &[ToolStep],
    expected: &[ToolStep],
    judge: &ArgEquivalenceJudge,
) -> Result<f64, JudgeError> {
    if actual.len() != expected.len() {
        return Ok(0.0);
    }
    for (x, y) in actual.iter().zip(expected) {
        if x.name != y.name {
            return Ok(0.0);
        }
        if !judge.args_equivalent(&x.name, &x.args, &y.args).await? {
            return Ok(0.0);
        }
    }
    Ok(1.0)
}

/// Stable string form of a JSON value for cache keys (serde_json's `Map` is
/// key-ordered, so identical values serialise identically).
fn canonical(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Parse a judge reply's first non-empty line into a yes/no verdict.
fn parse_yes_no(text: &str) -> Result<bool, JudgeError> {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_uppercase();
    // Check NO/NOT/FALSE before YES so "NOT EQUIVALENT" reads as false.
    if line.starts_with("NO") || line.starts_with("NOT") || line.starts_with("FALSE") {
        Ok(false)
    } else if line.starts_with("YES") || line.starts_with("TRUE") || line.starts_with("EQUIVALENT")
    {
        Ok(true)
    } else {
        Err(JudgeError::Parse(format!(
            "arg-judge reply didn't start with YES/NO: {:?}",
            line
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tars_pipeline::{Pipeline, ProviderService};
    use tars_provider::{CannedResponse, MockProvider};

    fn judge_with(reply: &str) -> (ArgEquivalenceJudge, Arc<MockProvider>) {
        let mock = MockProvider::new("judge_mock", CannedResponse::text(reply));
        let inner: Arc<dyn LlmService> = ProviderService::new(mock.clone());
        let svc = Arc::new(Pipeline::builder_with_inner(inner).build());
        (
            ArgEquivalenceJudge::new(svc, "judge_mock:m", ModelHint::Explicit("m".into())),
            mock,
        )
    }

    #[test]
    fn parse_yes_no_is_forgiving_but_strict() {
        assert_eq!(parse_yes_no("YES").unwrap(), true);
        assert_eq!(parse_yes_no("yes, they match\nbecause…").unwrap(), true);
        assert_eq!(parse_yes_no("NO").unwrap(), false);
        assert_eq!(parse_yes_no("Not equivalent — different query").unwrap(), false);
        assert!(parse_yes_no("maybe?").is_err());
    }

    #[tokio::test]
    async fn byte_equal_args_short_circuit_without_calling_the_model() {
        let (judge, mock) = judge_with("NO"); // model would say NO, but we never ask
        let same = json!({"q": "ducks"});
        assert!(judge.args_equivalent("search", &same, &same).await.unwrap());
        assert_eq!(mock.call_count(), 0, "byte-equal must not call the judge");
    }

    #[tokio::test]
    async fn differing_args_are_judged_and_cached() {
        let (judge, mock) = judge_with("YES");
        let a = json!({"q": "ducks"});
        let b = json!({"q": "duck"});
        assert!(judge.args_equivalent("search", &a, &b).await.unwrap());
        // Second call (even with args swapped) hits the symmetric cache → no
        // extra model call.
        assert!(judge.args_equivalent("search", &b, &a).await.unwrap());
        assert_eq!(mock.call_count(), 1, "verdict must be cached symmetrically");
    }

    #[tokio::test]
    async fn args_match_judged_is_all_or_nothing() {
        let (judge, _m) = judge_with("YES");
        let step = |n: &str, a: Value| ToolStep { name: n.into(), args: a };
        // names align, args differ but judge says YES → 1.0
        let actual = vec![step("search", json!({"q": "duck"}))];
        let expected = vec![step("search", json!({"q": "ducks"}))];
        assert_eq!(args_match_judged(&actual, &expected, &judge).await.unwrap(), 1.0);
        // name mismatch → 0.0 regardless of judge
        let wrong = vec![step("fetch", json!({"q": "duck"}))];
        assert_eq!(args_match_judged(&wrong, &expected, &judge).await.unwrap(), 0.0);

        // judge says NO → 0.0
        let (judge_no, _m2) = judge_with("NO");
        assert_eq!(args_match_judged(&actual, &expected, &judge_no).await.unwrap(), 0.0);
    }
}
