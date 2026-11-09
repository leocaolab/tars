//! Asks an LLM whether two byte-different tool-argument sets mean the same thing
//! (`search("ducks")` and `search("duck")`), for the `args-judge` trajectory check.
//!
//! The pure scorer in [`crate::trajectory_match`] knows nothing of it; the LLM call lives here.
//! Byte-equal arguments are equal without a call, and each verdict is cached for the pair in
//! either order. Refusing a judge that shares the run's provider is the caller's job
//! ([`crate::judge::check_providers`]).

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use tars_llm::LlmService;
use tars_types::{Caller, ChatRequest};

use crate::judge::{JudgeError, fill_template, first_line, first_word, reply_text};
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
    service: LlmService,
    id: String,
    caller: Caller,
    /// `(tool, lo, hi) -> equivalent?`, where `lo`/`hi` are the canonical-JSON
    /// strings of the two arg sets, ordered so `(a,b)` and `(b,a)` share a slot.
    cache: Mutex<HashMap<(String, String, String), bool>>,
}

impl ArgEquivalenceJudge {
    /// Builds a judge over `service`, named `id` (typically `"provider:model"`), whose every call
    /// is made for `caller`.
    pub fn new(service: LlmService, id: impl Into<String>, caller: Caller) -> Self {
        Self {
            service,
            id: id.into(),
            caller,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns whether `a` and `b` are equivalent arguments for `tool`, asking the model at most
    /// once per pair.
    ///
    /// # Errors
    ///
    /// Fails when the call fails, the reply runs past 10 000 events, or its first line is not a
    /// yes or a no.
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
        let key = if ca <= cb {
            (tool.to_string(), ca.clone(), cb.clone())
        } else {
            (tool.to_string(), cb.clone(), ca.clone())
        };
        if let Some(v) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied()
        {
            return Ok(v);
        }

        let prompt = fill_template(
            ARG_JUDGE_PROMPT,
            &[("{tool}", tool), ("{a}", &ca), ("{b}", &cb)],
        );
        let req = ChatRequest::user(prompt);
        let text = reply_text(&self.service, req, self.caller.for_call()).await?;
        let verdict = parse_yes_no(&text)?;
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, verdict);
        Ok(verdict)
    }
}

/// Scores `actual` against `expected` like `MatchMode::Args`, but counts byte-different
/// arguments as equal when the judge calls them equivalent: `1.0` iff the names align and every
/// argument pair is equal or judged equivalent, else `0.0`.
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

/// Returns a JSON value's string form for a cache key; serde_json's `Map` is key-ordered, so equal
/// values give equal strings.
fn canonical(v: &Value) -> String {
    v.to_string()
}

/// Reads a yes or a no from the first word of the judge reply's first non-empty line, ignoring
/// case and markdown. Only the words `yes` and `no` count: a reply that opens with another word
/// (`Not sure`, `Equivalent? No`) is not guessed at.
///
/// # Errors
///
/// Returns [`JudgeError::UnrecognizedEquivalence`] when that word is neither.
fn parse_yes_no(text: &str) -> Result<bool, JudgeError> {
    let line = first_line(text);
    match first_word(line).0.to_ascii_uppercase().as_str() {
        "NO" => Ok(false),
        "YES" => Ok(true),
        _ => Err(JudgeError::UnrecognizedEquivalence {
            first_line: line.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tars_llm::LlmService;
    use tars_llm::{CannedResponse, MockProvider};
    use tars_types::ProviderId;

    fn judge_with(reply: &str) -> (ArgEquivalenceJudge, Arc<MockProvider>) {
        let mock = MockProvider::new(ProviderId::new("judge_mock"), CannedResponse::text(reply));
        let inner = LlmService::of(mock.clone(), "gpt-4o");
        let svc = LlmService::builder_with_inner(inner).build();
        let judge = ArgEquivalenceJudge::new(svc, "judge_mock:m", Caller::test_default());
        (judge, mock)
    }

    #[tokio::test]
    async fn judged_args_are_asked_for_the_tenant_the_caller_names() {
        use tars_types::{TenantId, TraceId};

        let recorder = crate::test_support::CallRecorder::new("judge_rec", "YES");
        let svc = LlmService::of(recorder.clone(), "m");
        let caller = Caller::new(TenantId::new("args-tenant"), TraceId::new("t"));
        let judge = ArgEquivalenceJudge::new(svc, "judge_rec:m", caller);

        let equivalent = judge
            .args_equivalent("search", &json!({"q": "ducks"}), &json!({"q": "duck"}))
            .await
            .unwrap();

        assert!(equivalent);
        assert_eq!(recorder.tenants(), vec![TenantId::new("args-tenant")]);
    }

    #[test]
    fn a_standalone_yes_or_no_opening_the_reply_is_read_as_said() {
        for (reply, equivalent) in [
            ("YES", true),
            ("yes, they match\nbecause…", true),
            ("**Yes**", true),
            ("NO", false),
            ("no.", false),
            ("\n  No — different query", false),
        ] {
            assert_eq!(parse_yes_no(reply).unwrap(), equivalent, "{reply}");
        }
    }

    #[test]
    fn a_reply_that_opens_with_any_other_word_is_not_guessed() {
        for reply in [
            "Not sure",
            "Not equivalent — different query",
            "Equivalent? No",
            "True",
            "False",
            "Nope",
            "Yesterday's value",
            "maybe?",
        ] {
            assert!(
                matches!(
                    parse_yes_no(reply),
                    Err(JudgeError::UnrecognizedEquivalence { ref first_line }) if first_line == reply
                ),
                "{reply}"
            );
        }
    }

    #[test]
    fn a_word_that_only_starts_like_no_is_not_a_no() {
        assert!(matches!(
            parse_yes_no("Note: the second one says YES"),
            Err(JudgeError::UnrecognizedEquivalence { first_line }) if first_line == "Note: the second one says YES"
        ));
        assert!(parse_yes_no("**YES**").unwrap());
    }

    #[tokio::test]
    async fn an_argument_that_spells_a_placeholder_is_shown_to_the_judge_as_written() {
        let recorder = crate::test_support::CallRecorder::new("judge_rec", "NO");
        let svc = LlmService::of(recorder.clone(), "m");
        let judge = ArgEquivalenceJudge::new(svc, "judge_rec:m", Caller::test_default());

        judge
            .args_equivalent(
                "render",
                &json!({"template": "{b}"}),
                &json!({"template": "hello"}),
            )
            .await
            .unwrap();

        let prompt = &recorder.prompts()[0];
        assert!(
            prompt.contains("Arguments A: {\"template\":\"{b}\"}"),
            "{prompt}"
        );
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
        let step = |n: &str, a: Value| ToolStep {
            name: n.into(),
            args: a,
        };
        // names align, args differ but judge says YES → 1.0
        let actual = vec![step("search", json!({"q": "duck"}))];
        let expected = vec![step("search", json!({"q": "ducks"}))];
        assert_eq!(
            args_match_judged(&actual, &expected, &judge).await.unwrap(),
            1.0
        );
        // name mismatch → 0.0 regardless of judge
        let wrong = vec![step("fetch", json!({"q": "duck"}))];
        assert_eq!(
            args_match_judged(&wrong, &expected, &judge).await.unwrap(),
            0.0
        );

        // judge says NO → 0.0
        let (judge_no, _m2) = judge_with("NO");
        assert_eq!(
            args_match_judged(&actual, &expected, &judge_no)
                .await
                .unwrap(),
            0.0
        );
    }
}
