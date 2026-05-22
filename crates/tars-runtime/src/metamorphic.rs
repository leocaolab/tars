//! `MetamorphicRelation` + `GoldenMatch` + `Mutation` — the remaining
//! oracle-free test dimensions from Doc 18 (§4.2 / §4.4 / §4.3).
//!
//! These are **library traits + generic helpers**. The CLI run-modes
//! that exercise them against a live LLM (`tars eval metamorphic` etc.)
//! are follow-on wiring; the traits + their pure logic live here and
//! are unit-tested without a provider.

use std::sync::Arc;

use tars_types::{ChatRequest, ChatResponse};

use crate::check::CheckResult;

// ─── §4.2 Metamorphic relations ───────────────────────────────────────

/// A relation between a base run and a transformed run. `transform`
/// produces a new request from the base; `relation_holds` checks the
/// two responses satisfy the expected relation. No oracle: you never
/// need the right answer, only how it must behave under the transform.
pub trait MetamorphicRelation: Send + Sync {
    fn name(&self) -> &str;
    /// Derive the transformed request from the base.
    fn transform(&self, base: &ChatRequest) -> ChatRequest;
    /// Check the relation between the base output and the transformed
    /// output.
    fn relation_holds(&self, base: &ChatResponse, transformed: &ChatResponse) -> CheckResult;
}

/// Invariance relation built from a text transform + an equivalence
/// predicate: after transforming the input, the output must stay
/// "equivalent" by the caller's definition. Covers paraphrase /
/// reorder / rename / vary-distance (Doc 18 §4.2 INV).
pub struct InvarianceRelation {
    name: String,
    transform_text: Box<dyn Fn(&str) -> String + Send + Sync>,
    equivalent: Box<dyn Fn(&str, &str) -> bool + Send + Sync>,
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

    /// Determinism check: transform is identity, equivalence is exact
    /// string equality. "Same input twice → same output."
    pub fn determinism() -> Self {
        Self::new("determinism", |s| s.to_string(), |a, b| a == b)
    }
}

impl MetamorphicRelation for InvarianceRelation {
    fn name(&self) -> &str {
        &self.name
    }

    fn transform(&self, base: &ChatRequest) -> ChatRequest {
        // Rewrite the last user text via transform_text; other turns
        // pass through. Most invariance tests perturb the user prompt.
        let mut req = base.clone();
        if let Some(last_user_text) = req
            .messages
            .iter_mut()
            .rev()
            .find_map(|m| match m {
                tars_types::Message::User { content } => content
                    .iter_mut()
                    .rev()
                    .find_map(|b| match b {
                        tars_types::ContentBlock::Text { text } => Some(text),
                        _ => None,
                    }),
                _ => None,
            })
        {
            *last_user_text = (self.transform_text)(last_user_text);
        }
        req
    }

    fn relation_holds(&self, base: &ChatResponse, transformed: &ChatResponse) -> CheckResult {
        if (self.equivalent)(&base.text, &transformed.text) {
            CheckResult::pass()
        } else {
            CheckResult::fail(format!(
                "outputs not equivalent under `{}`: {:?} vs {:?}",
                self.name,
                truncate(&base.text, 60),
                truncate(&transformed.text, 60),
            ))
        }
    }
}

/// Directional relation: after the transform, the output must change a
/// known way. Caller supplies the directional predicate over (base,
/// transformed). Covers add-constraint → shrink, negate → flip, etc.
/// (Doc 18 §4.2 DIR).
pub struct DirectionalRelation {
    name: String,
    transform_text: Box<dyn Fn(&str) -> String + Send + Sync>,
    direction_holds: Box<dyn Fn(&str, &str) -> bool + Send + Sync>,
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
            tars_types::Message::User { content } => content.iter_mut().rev().find_map(|b| {
                match b {
                    tars_types::ContentBlock::Text { text } => Some(text),
                    _ => None,
                }
            }),
            _ => None,
        }) {
            *t = (self.transform_text)(t);
        }
        req
    }

    fn relation_holds(&self, base: &ChatResponse, transformed: &ChatResponse) -> CheckResult {
        if (self.direction_holds)(&base.text, &transformed.text) {
            CheckResult::pass()
        } else {
            CheckResult::fail(format!("directional relation `{}` violated", self.name))
        }
    }
}

// ─── §4.4 Golden match ─────────────────────────────────────────────────

/// How to compare an output against an approved golden snapshot.
/// Exact/structural are oracle-free string/JSON checks; the semantic
/// variant (delegating to a judge) is built at the CLI layer where the
/// judge lives.
pub enum GoldenMatch {
    /// Byte-for-byte string equality (deterministic outputs).
    Exact,
    /// Parse both as JSON and compare the *shape* (key sets), ignoring
    /// scalar values. Tolerates content variation, catches schema drift.
    StructuralJson,
}

impl GoldenMatch {
    /// Compare `output` against `golden`. Returns a [`CheckResult`]:
    /// pass = matches golden (no drift), fail = drifted.
    pub fn compare(&self, golden: &str, output: &str) -> CheckResult {
        match self {
            GoldenMatch::Exact => {
                if golden == output {
                    CheckResult::pass()
                } else {
                    CheckResult::fail("output differs from golden (exact)")
                }
            }
            GoldenMatch::StructuralJson => {
                let g: Result<serde_json::Value, _> = serde_json::from_str(golden);
                let o: Result<serde_json::Value, _> = serde_json::from_str(output);
                match (g, o) {
                    (Ok(gv), Ok(ov)) => {
                        if json_shape(&gv) == json_shape(&ov) {
                            CheckResult::pass()
                        } else {
                            CheckResult::fail("JSON shape differs from golden")
                        }
                    }
                    _ => CheckResult::fail("golden or output is not valid JSON"),
                }
            }
        }
    }
}

/// Reduce a JSON value to its shape: objects → sorted key set with
/// recursively-shaped values; arrays → shape of first element (or
/// empty); scalars → their type tag. Ignores scalar *values*.
fn json_shape(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<String> = m
                .iter()
                .map(|(k, val)| format!("{k}:{}", json_shape(val)))
                .collect();
            keys.sort();
            format!("{{{}}}", keys.join(","))
        }
        serde_json::Value::Array(a) => {
            let inner = a.first().map(json_shape).unwrap_or_else(|| "_".into());
            format!("[{inner}]")
        }
        serde_json::Value::String(_) => "s".into(),
        serde_json::Value::Number(_) => "n".into(),
        serde_json::Value::Bool(_) => "b".into(),
        serde_json::Value::Null => "z".into(),
    }
}

// ─── §4.3 Mutation ─────────────────────────────────────────────────────

/// A system mutation paired with the check it's expected to break.
/// Mutation testing for the eval suite (Doc 18 §4.3b): apply the
/// mutation, re-run the eval, and confirm the named check's violation
/// rate rose. If it didn't, the eval is blind to that regression class.
///
/// The mutation operates on a prompt string (the most common system
/// knob); `expected_to_break` names the check that should start
/// failing. The harness that applies + re-runs lives at the CLI/eval
/// layer; this trait is the contract.
pub trait Mutation: Send + Sync {
    fn name(&self) -> &str;
    /// Mutate a prompt (e.g. delete a "cite sources" instruction).
    fn mutate_prompt(&self, prompt: &str) -> String;
    /// The check id that this mutation should cause to start failing.
    fn expected_to_break(&self) -> &str;
}

/// Mutation that deletes a marker line/substring from the prompt — the
/// canonical "remove an instruction and see if the eval notices."
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

/// Verdict of a mutation test: did the eval catch the injected
/// regression? `caught = baseline_violation_rate < mutated_violation_rate`
/// for the expected-to-break check.
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
    /// True = the eval's relevant check noticed the mutation (good — the
    /// eval can catch this regression class). False = the eval is blind.
    pub caught: bool,
    pub baseline_rate: f64,
    pub mutated_rate: f64,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_types::{ChatEvent, ChatRequest, ChatResponseBuilder, ModelHint, StopReason, Usage};

    fn user_req(text: &str) -> ChatRequest {
        ChatRequest::user(ModelHint::Explicit("m".into()), text)
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
        let rel = InvarianceRelation::new(
            "uppercase",
            |s| s.to_uppercase(),
            |a, b| a == b,
        );
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
        assert!(rel.relation_holds(&resp("Drive"), &resp("drive")).passed);
        assert!(!rel.relation_holds(&resp("drive"), &resp("walk")).passed);
    }

    #[test]
    fn determinism_relation() {
        let rel = InvarianceRelation::determinism();
        assert!(rel.relation_holds(&resp("x"), &resp("x")).passed);
        assert!(!rel.relation_holds(&resp("x"), &resp("y")).passed);
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
                .passed
        );
        assert!(
            !rel.relation_holds(&resp("short"), &resp("a very long answer indeed"))
                .passed
        );
    }

    #[test]
    fn golden_exact_match() {
        let g = GoldenMatch::Exact;
        assert!(g.compare("hello", "hello").passed);
        assert!(!g.compare("hello", "hullo").passed);
    }

    #[test]
    fn golden_structural_ignores_values_catches_shape() {
        let g = GoldenMatch::StructuralJson;
        // same shape, different values → pass (no drift)
        assert!(
            g.compare(r#"{"a":1,"b":"x"}"#, r#"{"a":99,"b":"different"}"#)
                .passed
        );
        // missing key → shape drift → fail
        assert!(!g.compare(r#"{"a":1,"b":2}"#, r#"{"a":1}"#).passed);
        // key order doesn't matter
        assert!(g.compare(r#"{"a":1,"b":2}"#, r#"{"b":5,"a":7}"#).passed);
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
