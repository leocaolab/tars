//! Defines the oracle-free tests other than invariants (`docs/architecture/18-agent-testing.md`
//! §4.2–§4.4): metamorphic relations between two outputs, matching an output against a golden,
//! and mutations an eval is expected to catch.
//!
//! They are library pieces only: nothing in tars runs them against a live model, and no
//! `tars eval` command calls them.

use tars_types::{ChatRequest, ChatResponse};

use crate::check::CheckOutcome;
use crate::truncate;

// ─── Metamorphic relations ────────────────────────────────────────────

/// Says how an output must behave when its request is transformed, without needing the right
/// answer.
pub trait MetamorphicRelation: Send + Sync {
    fn name(&self) -> &str;
    fn transform(&self, base: &ChatRequest) -> ChatRequest;
    fn relation_holds(&self, base: &ChatResponse, transformed: &ChatResponse) -> CheckOutcome;
}

type TransformFn = Box<dyn Fn(&str) -> String + Send + Sync>;
type RelationFn = Box<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// Requires the output to stay equivalent, by the caller's predicate, after the last user text is
/// transformed (paraphrase, reorder, rename).
pub struct InvarianceRelation {
    name: String,
    transform_text: TransformFn,
    equivalent: RelationFn,
}

impl InvarianceRelation {
    pub fn new(
        name: impl Into<String>,
        transform_text: impl Fn(&str) -> String + Send + Sync + 'static,
        equivalent: impl Fn(&str, &str) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            transform_text: Box::new(transform_text),
            equivalent: Box::new(equivalent),
        }
    }

    /// Builds the relation that the same input twice gives the same output.
    pub fn determinism() -> Self {
        Self::new("determinism", |s| s.to_string(), |a, b| a == b)
    }
}

impl MetamorphicRelation for InvarianceRelation {
    fn name(&self) -> &str {
        &self.name
    }

    fn transform(&self, base: &ChatRequest) -> ChatRequest {
        let mut req = base.clone();
        if let Some(last_user_text) = req.messages.iter_mut().rev().find_map(|m| match m {
            tars_types::Message::User { content } => {
                content.iter_mut().rev().find_map(|b| match b {
                    tars_types::ContentBlock::Text { text } => Some(text),
                    _ => None,
                })
            }
            _ => None,
        }) {
            *last_user_text = (self.transform_text)(last_user_text);
        }
        req
    }

    fn relation_holds(&self, base: &ChatResponse, transformed: &ChatResponse) -> CheckOutcome {
        if (self.equivalent)(&base.text, &transformed.text) {
            CheckOutcome::pass()
        } else {
            CheckOutcome::fail(format!(
                "outputs not equivalent under `{}`: {:?} vs {:?}",
                self.name,
                truncate(&base.text, 60),
                truncate(&transformed.text, 60),
            ))
        }
    }
}

/// Requires the output to change a known way after the last user text is transformed (add a
/// constraint and it shrinks, negate and it flips).
pub struct DirectionalRelation {
    name: String,
    transform_text: TransformFn,
    direction_holds: RelationFn,
}

impl DirectionalRelation {
    pub fn new(
        name: impl Into<String>,
        transform_text: impl Fn(&str) -> String + Send + Sync + 'static,
        direction_holds: impl Fn(&str, &str) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            transform_text: Box::new(transform_text),
            direction_holds: Box::new(direction_holds),
        }
    }
}

impl MetamorphicRelation for DirectionalRelation {
    fn name(&self) -> &str {
        &self.name
    }

    fn transform(&self, base: &ChatRequest) -> ChatRequest {
        let mut req = base.clone();
        if let Some(t) = req.messages.iter_mut().rev().find_map(|m| match m {
            tars_types::Message::User { content } => {
                content.iter_mut().rev().find_map(|b| match b {
                    tars_types::ContentBlock::Text { text } => Some(text),
                    _ => None,
                })
            }
            _ => None,
        }) {
            *t = (self.transform_text)(t);
        }
        req
    }

    fn relation_holds(&self, base: &ChatResponse, transformed: &ChatResponse) -> CheckOutcome {
        if (self.direction_holds)(&base.text, &transformed.text) {
            CheckOutcome::pass()
        } else {
            CheckOutcome::fail(format!("directional relation `{}` violated", self.name))
        }
    }
}

// ─── Golden match ─────────────────────────────────────────────────────

/// How to compare an output against an approved golden snapshot, without a judge.
pub enum GoldenMatch {
    /// Byte-for-byte string equality (deterministic outputs).
    Exact,
    /// Parse both as JSON and compare the *shape* (key sets), ignoring
    /// scalar values. Tolerates content variation, catches schema drift.
    StructuralJson,
}

impl GoldenMatch {
    pub fn compare(&self, golden: &str, output: &str) -> CheckOutcome {
        match self {
            GoldenMatch::Exact => {
                if golden == output {
                    CheckOutcome::pass()
                } else {
                    CheckOutcome::fail("output differs from golden (exact)")
                }
            }
            GoldenMatch::StructuralJson => {
                let g: Result<serde_json::Value, _> = serde_json::from_str(golden);
                let o: Result<serde_json::Value, _> = serde_json::from_str(output);
                match (g, o) {
                    (Ok(gv), Ok(ov)) => {
                        if same_shape(&gv, &ov) {
                            CheckOutcome::pass()
                        } else {
                            CheckOutcome::fail("JSON shape differs from golden")
                        }
                    }
                    _ => CheckOutcome::fail("golden or output is not valid JSON"),
                }
            }
        }
    }
}

/// Says whether two JSON values have one shape: objects with the same keys, each of one shape;
/// arrays whose first elements are of one shape (both empty counts); scalars of one type. Only
/// the first element of an array counts.
fn same_shape(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value as V;
    match (a, b) {
        (V::Object(x), V::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, xv)| y.get(k).is_some_and(|yv| same_shape(xv, yv)))
        }
        (V::Array(x), V::Array(y)) => match (x.first(), y.first()) {
            (Some(xv), Some(yv)) => same_shape(xv, yv),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        },
        (V::String(_), V::String(_))
        | (V::Number(_), V::Number(_))
        | (V::Bool(_), V::Bool(_))
        | (V::Null, V::Null) => true,
        _ => false,
    }
}

// ─── Mutation ─────────────────────────────────────────────────────────

/// A change to the prompt paired with the check it should break: apply it, re-run the eval, and
/// see whether that check's violation rate rose. If it did not, the eval is blind to that
/// regression class. Applying and re-running is the caller's.
pub trait Mutation: Send + Sync {
    fn name(&self) -> &str;
    fn mutate_prompt(&self, prompt: &str) -> String;
    fn expected_to_break(&self) -> &str;
}

/// Deletes every occurrence of a substring from the prompt: remove an instruction and see whether
/// the eval notices.
pub struct DeleteSubstringMutation {
    name: String,
    needle: String,
    expected_break: String,
}

impl DeleteSubstringMutation {
    pub fn new(
        name: impl Into<String>,
        needle: impl Into<String>,
        expected_break: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            needle: needle.into(),
            expected_break: expected_break.into(),
        }
    }
}

impl Mutation for DeleteSubstringMutation {
    fn name(&self) -> &str {
        &self.name
    }
    fn mutate_prompt(&self, prompt: &str) -> String {
        prompt.replace(&self.needle, "")
    }
    fn expected_to_break(&self) -> &str {
        &self.expected_break
    }
}

/// Returns whether the eval caught a mutation: the expected-to-break check's violation rate rose
/// above the baseline by more than 1e-9.
pub fn mutation_caught(baseline_rate: f64, mutated_rate: f64) -> MutationVerdict {
    let caught = mutated_rate > baseline_rate + 1e-9;
    MutationVerdict {
        caught,
        baseline_rate,
        mutated_rate,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MutationVerdict {
    pub caught: bool,
    pub baseline_rate: f64,
    pub mutated_rate: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::{ChatEvent, ChatRequest, ChatResponseBuilder, StopReason, Usage};

    fn user_req(text: &str) -> ChatRequest {
        ChatRequest::user(text)
    }

    fn resp(text: &str) -> ChatResponse {
        let mut b = ChatResponseBuilder::new();
        b.apply(ChatEvent::started("m"));
        b.apply(ChatEvent::Delta { text: text.into() });
        b.apply(ChatEvent::Finished {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        });
        b.finish()
    }

    #[test]
    fn invariance_transform_rewrites_user_text() {
        let rel = InvarianceRelation::new("uppercase", |s| s.to_uppercase(), |a, b| a == b);
        let t = rel.transform(&user_req("hello world"));
        let txt = t.messages[0].content()[0].as_text().unwrap();
        assert_eq!(txt, "HELLO WORLD");
    }

    #[test]
    fn invariance_relation_holds_and_fails() {
        // equivalence = case-insensitive
        let rel = InvarianceRelation::new(
            "paraphrase",
            |s| s.to_string(),
            |a, b| a.eq_ignore_ascii_case(b),
        );
        assert!(rel.relation_holds(&resp("Drive"), &resp("drive")).passed());
        assert!(!rel.relation_holds(&resp("drive"), &resp("walk")).passed());
    }

    #[test]
    fn determinism_relation() {
        let rel = InvarianceRelation::determinism();
        assert!(rel.relation_holds(&resp("x"), &resp("x")).passed());
        assert!(!rel.relation_holds(&resp("x"), &resp("y")).passed());
    }

    #[test]
    fn directional_relation_checks_direction() {
        // add-constraint → output must be shorter
        let rel = DirectionalRelation::new(
            "add_length_constraint",
            |s| format!("{s} (under 5 words)"),
            |base, transformed| transformed.len() < base.len(),
        );
        assert!(
            rel.relation_holds(&resp("a very long answer indeed"), &resp("short"))
                .passed()
        );
        assert!(
            !rel.relation_holds(&resp("short"), &resp("a very long answer indeed"))
                .passed()
        );
    }

    #[test]
    fn golden_exact_match() {
        let g = GoldenMatch::Exact;
        assert!(g.compare("hello", "hello").passed());
        assert!(!g.compare("hello", "hullo").passed());
    }

    #[test]
    fn golden_structural_ignores_values_catches_shape() {
        let g = GoldenMatch::StructuralJson;
        // same shape, different values → pass (no drift)
        assert!(
            g.compare(r#"{"a":1,"b":"x"}"#, r#"{"a":99,"b":"different"}"#)
                .passed()
        );
        // missing key → shape drift → fail
        assert!(!g.compare(r#"{"a":1,"b":2}"#, r#"{"a":1}"#).passed());
        // key order doesn't matter
        assert!(g.compare(r#"{"a":1,"b":2}"#, r#"{"b":5,"a":7}"#).passed());
    }

    #[test]
    fn keys_that_spell_another_shape_do_not_match_it() {
        let g = GoldenMatch::StructuralJson;
        assert!(!g.compare(r#"{"a":"x","b":1}"#, r#"{"a:s,b":1}"#).passed());
        assert!(!g.compare(r#"{"a":[]}"#, r#"{"a":[1]}"#).passed());
        assert!(g.compare(r#"{"a":[1,"x"]}"#, r#"{"a":[2]}"#).passed());
    }

    #[test]
    fn mutation_delete_substring() {
        let m = DeleteSubstringMutation::new(
            "drop_cite_instruction",
            "Always cite sources.",
            "grounding",
        );
        let mutated = m.mutate_prompt("Be concise. Always cite sources. Use markdown.");
        assert!(!mutated.contains("cite sources"));
        assert_eq!(m.expected_to_break(), "grounding");
    }

    #[test]
    fn mutation_caught_when_violation_rises() {
        // baseline grounding violations 2%, mutated 40% → eval caught it
        let caught = mutation_caught(0.02, 0.40);
        assert!(caught.caught);
        // baseline == mutated → eval is BLIND to this mutation
        let blind = mutation_caught(0.02, 0.02);
        assert!(!blind.caught);
    }
}
