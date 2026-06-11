//! Tool-trajectory scoring — [Doc 26](../../../docs/architecture/26-tool-trajectory-eval.md).
//!
//! Pure, no I/O, no LLM: given the tool steps an agent *selected* and a
//! reference list, score how well they agree. This is the tars analogue of
//! Google ADK's `tool_trajectory_avg_score`, extended with partial-credit
//! (`Ordered`) and order-insensitive (`Set`) modes.
//!
//! P1 scores **names only** — the call sequence of tool names. Argument
//! matching is a later phase (Doc 26 P3).

use serde_json::Value;

/// One step of a tool trajectory: a tool name + the args it was called with.
/// P1 scoring uses only `name`; `args` is carried for the P3 `args` mode.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolStep {
    pub name: String,
    pub args: Value,
}

/// How two tool-name sequences are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchMode {
    /// 1.0 iff the name sequences are identical, else 0.0 (ADK semantics).
    Exact,
    /// Dice coefficient over the longest common subsequence — partial credit
    /// for prefix / subsequence agreement. `2·LCS / (|a| + |b|)`.
    Ordered,
    /// Jaccard over the tool-name *multiset* — order-insensitive
    /// ("did they reach for the same tools, regardless of order").
    Set,
}

impl MatchMode {
    /// Parse the mode token of a `trajectory-match:<mode>` spec.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "exact" => Some(Self::Exact),
            "ordered" => Some(Self::Ordered),
            "set" => Some(Self::Set),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Ordered => "ordered",
            Self::Set => "set",
        }
    }
}

/// Adapt a response's tool calls into the scorer's step view.
pub fn from_tool_calls(calls: &[tars_types::ToolCall]) -> Vec<ToolStep> {
    calls
        .iter()
        .map(|c| ToolStep {
            name: c.name.clone(),
            args: c.arguments.clone(),
        })
        .collect()
}

/// Score `actual` against `expected` under `mode`. Always in `[0.0, 1.0]`;
/// 1.0 = perfect agreement. Total: never panics, empty-vs-empty = 1.0.
pub fn score(actual: &[ToolStep], expected: &[ToolStep], mode: MatchMode) -> f64 {
    let a: Vec<&str> = actual.iter().map(|s| s.name.as_str()).collect();
    let b: Vec<&str> = expected.iter().map(|s| s.name.as_str()).collect();
    score_names(&a, &b, mode)
}

/// Score two tool-name sequences directly — used by the head-to-head
/// `eval diff --trajectory` path over the persisted `tool_trajectory`
/// (names), where there are no `ToolStep`s to rebuild.
pub fn score_names(a: &[&str], b: &[&str], mode: MatchMode) -> f64 {
    match mode {
        MatchMode::Exact => {
            if a == b {
                1.0
            } else {
                0.0
            }
        }
        MatchMode::Set => jaccard_multiset(a, b),
        MatchMode::Ordered => {
            if a.is_empty() && b.is_empty() {
                return 1.0;
            }
            let l = lcs_len(a, b);
            (2.0 * l as f64) / (a.len() + b.len()) as f64
        }
    }
}

/// Classic O(n·m) LCS length with a single rolling row.
fn lcs_len(a: &[&str], b: &[&str]) -> usize {
    let m = b.len();
    let mut dp = vec![0usize; m + 1];
    for ai in a {
        let mut prev = 0; // dp[j-1] from the previous row
        for j in 1..=m {
            let tmp = dp[j];
            dp[j] = if *ai == b[j - 1] {
                prev + 1
            } else {
                dp[j].max(dp[j - 1])
            };
            prev = tmp;
        }
    }
    dp[m]
}

/// Multiset Jaccard: `Σ min(count) / Σ max(count)`. Empty-vs-empty = 1.0.
fn jaccard_multiset(a: &[&str], b: &[&str]) -> f64 {
    use std::collections::{HashMap, HashSet};
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut ca: HashMap<&str, i64> = HashMap::new();
    let mut cb: HashMap<&str, i64> = HashMap::new();
    for x in a {
        *ca.entry(x).or_default() += 1;
    }
    for x in b {
        *cb.entry(x).or_default() += 1;
    }
    let keys: HashSet<&str> = ca.keys().chain(cb.keys()).copied().collect();
    let (mut inter, mut uni) = (0i64, 0i64);
    for k in keys {
        let (va, vb) = (*ca.get(k).unwrap_or(&0), *cb.get(k).unwrap_or(&0));
        inter += va.min(vb);
        uni += va.max(vb);
    }
    if uni == 0 {
        1.0
    } else {
        inter as f64 / uni as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn steps(names: &[&str]) -> Vec<ToolStep> {
        names
            .iter()
            .map(|n| ToolStep {
                name: (*n).to_string(),
                args: json!({}),
            })
            .collect()
    }

    // E2E-4 (unit): exact / ordered / set on identical, reordered, subset,
    // disjoint, empty pairs → expected fractions.
    #[test]
    fn exact_is_all_or_nothing() {
        assert_eq!(score(&steps(&["a", "b"]), &steps(&["a", "b"]), MatchMode::Exact), 1.0);
        assert_eq!(score(&steps(&["a", "b"]), &steps(&["b", "a"]), MatchMode::Exact), 0.0);
        assert_eq!(score(&steps(&["a"]), &steps(&["a", "b"]), MatchMode::Exact), 0.0);
    }

    #[test]
    fn ordered_gives_partial_credit_via_lcs() {
        // identical → 1.0
        assert_eq!(score(&steps(&["a", "b", "c"]), &steps(&["a", "b", "c"]), MatchMode::Ordered), 1.0);
        // one of two shared in order: LCS=1, 2*1/(2+2)=0.5
        assert_eq!(score(&steps(&["a", "x"]), &steps(&["a", "y"]), MatchMode::Ordered), 0.5);
        // subset prefix: actual [a], expected [a,b]: LCS=1, 2/(1+2)=0.666..
        let s = score(&steps(&["a"]), &steps(&["a", "b"]), MatchMode::Ordered);
        assert!((s - 2.0 / 3.0).abs() < 1e-9);
        // reorder costs in ordered: [a,b] vs [b,a]: LCS=1, 2/4=0.5
        assert_eq!(score(&steps(&["a", "b"]), &steps(&["b", "a"]), MatchMode::Ordered), 0.5);
    }

    #[test]
    fn set_is_order_insensitive_multiset() {
        // reorder → perfect under Set
        assert_eq!(score(&steps(&["a", "b"]), &steps(&["b", "a"]), MatchMode::Set), 1.0);
        // disjoint → 0
        assert_eq!(score(&steps(&["a"]), &steps(&["b"]), MatchMode::Set), 0.0);
        // {a,a,b} vs {a,b}: inter=min(2,1)+min(1,1)=2, uni=max+max=3 → 0.666..
        let s = score(&steps(&["a", "a", "b"]), &steps(&["a", "b"]), MatchMode::Set);
        assert!((s - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn empty_pairs_are_perfect_in_every_mode() {
        for m in [MatchMode::Exact, MatchMode::Ordered, MatchMode::Set] {
            assert_eq!(score(&[], &[], m), 1.0, "mode {:?}", m);
        }
        // empty actual vs non-empty expected → 0 (not a free pass)
        assert_eq!(score(&[], &steps(&["a"]), MatchMode::Ordered), 0.0);
        assert_eq!(score(&[], &steps(&["a"]), MatchMode::Set), 0.0);
    }

    #[test]
    fn mode_parse_roundtrips_and_rejects_unknown() {
        for m in [MatchMode::Exact, MatchMode::Ordered, MatchMode::Set] {
            assert_eq!(MatchMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(MatchMode::parse("fuzzy"), None);
    }
}
