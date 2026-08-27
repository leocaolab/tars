//! What changed between a recorded request and the live one — the answer a
//! cassette MISS owes its reader.
//!
//! A fingerprint says "different" and stops there, which leaves re-recording as
//! the only available move; re-recording an unexamined change stamps whatever
//! drifted — regression included — as the new baseline. This module turns that
//! into a located, reviewable record. Design: `docs/design/cassette-request-diff.md`.
//!
//! Two rules run through everything here:
//!   - **Omit what is identical, never truncate what differs.** The first is the
//!     definition of a diff; the second would throw away the record's only
//!     content.
//!   - **Fold for display, never for storage.** Folding announces its size and
//!     leaves the original reachable; truncation is silent and irreversible.

use std::collections::BTreeMap;

use serde_json::Value;

/// How the baseline recording was chosen. Printed with every diff: a diff
/// against the wrong baseline is worse than no diff, because it points at a
/// change that never happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineBy {
    /// The consumer's stable step label — the only deterministic option, and
    /// the only one that survives concurrent calls.
    Label,
    /// Position in the session. Sound only while the journey is serial.
    Seq,
    /// Longest shared prefix. A GUESS, and rendered as one.
    Prefix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Added,
    Removed,
    Changed,
}

/// One located difference. `old`/`new` carry the FULL value — folding happens
/// only when rendering.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Change {
    /// JSON Pointer (RFC 6901). Survives reformatting, unlike a line number,
    /// and lets a tool jump straight to the value.
    pub path: String,
    /// The request part this path lands in, when tars can name it. `None` when
    /// it cannot — a guessed component name is a fabricated clue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    pub op: Op,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    pub changed: usize,
    pub added: usize,
    pub removed: usize,
    /// How much of the request was identical and therefore omitted — so the
    /// reader knows this is a diff, not a snapshot.
    pub identical_bytes: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RequestDiff {
    pub kind: &'static str,
    pub version: u32,
    pub fingerprint: Fingerprints,
    pub baseline_selected_by: BaselineBy,
    pub changes: Vec<Change>,
    pub summary: Summary,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Fingerprints {
    pub want: String,
    pub baseline: String,
}

/// Display depth. Storage is always `Full`; these only shape a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fold {
    /// One line: counts + components + where the full record is.
    Summary,
    /// Each change located, values folded to their first/last lines.
    Folded,
    /// Every changed value in full.
    Full,
}

/// The request part a pointer lands in. Only the shapes tars owns
/// (`ChatRequest`) are named; anything else yields `None` rather than a guess.
fn component_of(path: &str) -> Option<&'static str> {
    let seg = |n: usize| path.split('/').nth(n);
    match seg(1)? {
        "system" => Some("system-prompt"),
        "messages" => Some("message"),
        "tools" => Some("tool-specs"),
        "response_format" | "schema" => Some("schema"),
        "model" => Some("model"),
        _ => None,
    }
}

fn esc(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

fn repr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Walk both values, emitting one [`Change`] per differing location.
/// Identical subtrees contribute nothing — that is what makes this a diff.
fn walk(a: &Value, b: &Value, path: &str, out: &mut Vec<Change>) {
    if a == b {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeMap<&String, ()> = x.keys().chain(y.keys()).map(|k| (k, ())).collect();
            for k in keys.into_keys() {
                let p = format!("{path}/{}", esc(k));
                match (x.get(k), y.get(k)) {
                    (Some(av), Some(bv)) => walk(av, bv, &p, out),
                    (Some(av), None) => out.push(Change {
                        component: component_of(&p).map(str::to_string),
                        path: p,
                        op: Op::Removed,
                        old: Some(repr(av)),
                        new: None,
                    }),
                    (None, Some(bv)) => out.push(Change {
                        component: component_of(&p).map(str::to_string),
                        path: p,
                        op: Op::Added,
                        old: None,
                        new: Some(repr(bv)),
                    }),
                    (None, None) => unreachable!("key came from one of the two maps"),
                }
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            for i in 0..x.len().max(y.len()) {
                let p = format!("{path}/{i}");
                match (x.get(i), y.get(i)) {
                    (Some(av), Some(bv)) => walk(av, bv, &p, out),
                    (Some(av), None) => out.push(Change {
                        component: component_of(&p).map(str::to_string),
                        path: p,
                        op: Op::Removed,
                        old: Some(repr(av)),
                        new: None,
                    }),
                    (None, Some(bv)) => out.push(Change {
                        component: component_of(&p).map(str::to_string),
                        path: p,
                        op: Op::Added,
                        old: None,
                        new: Some(repr(bv)),
                    }),
                    (None, None) => unreachable!("index below the longer length"),
                }
            }
        }
        _ => out.push(Change {
            component: component_of(path).map(str::to_string),
            path: path.to_string(),
            op: Op::Changed,
            old: Some(repr(a)),
            new: Some(repr(b)),
        }),
    }
}

impl RequestDiff {
    /// Diff two canonical request texts. Each is `model=<m>\0<json>`; the JSON
    /// half is compared structurally so a change reports WHERE it landed.
    /// Falls back to a single whole-body change when either side is not JSON —
    /// saying "the body changed" beats inventing structure that isn't there.
    pub fn build(want: &str, baseline: &str, fp: Fingerprints, by: BaselineBy) -> Self {
        let split = |s: &str| match s.split_once('\0') {
            Some((m, body)) => (m.trim_start_matches("model=").to_string(), body.to_string()),
            None => (String::new(), s.to_string()),
        };
        let (wm, wb) = split(want);
        let (bm, bb) = split(baseline);

        let mut changes = Vec::new();
        if wm != bm {
            changes.push(Change {
                path: "/model".into(),
                component: Some("model".into()),
                op: Op::Changed,
                old: Some(bm),
                new: Some(wm),
            });
        }
        match (
            serde_json::from_str::<Value>(&bb),
            serde_json::from_str::<Value>(&wb),
        ) {
            (Ok(a), Ok(b)) => walk(&a, &b, "", &mut changes),
            _ if bb != wb => changes.push(Change {
                path: "/".into(),
                component: None,
                op: Op::Changed,
                old: Some(bb.clone()),
                new: Some(wb.clone()),
            }),
            _ => {}
        }

        let identical_bytes = bb
            .bytes()
            .zip(wb.bytes())
            .take_while(|(x, y)| x == y)
            .count();
        let summary = Summary {
            changed: changes.iter().filter(|c| c.op == Op::Changed).count(),
            added: changes.iter().filter(|c| c.op == Op::Added).count(),
            removed: changes.iter().filter(|c| c.op == Op::Removed).count(),
            identical_bytes,
        };
        Self { kind: "cassette-request-diff", version: 1, fingerprint: fp, baseline_selected_by: by, changes, summary }
    }

    /// Build from the provider's typed miss.
    ///
    /// This is the seam the layering rests on: the provider hands over FACTS
    /// (both canonical requests, and how it chose the baseline) and stops; the
    /// testing layer decides what they mean and how to show them. `None` when
    /// the cassette captured no request — there is genuinely nothing to compare,
    /// and an empty diff would read as "nothing changed".
    pub fn from_miss(err: &tars_types::ProviderError) -> Option<Self> {
        let tars_types::ProviderError::CassetteMiss {
            want_fp,
            want_canon,
            baseline_fp,
            baseline_canon,
            baseline_selected_by,
        } = err
        else {
            return None;
        };
        let by = match baseline_selected_by.as_deref() {
            Some("label") => BaselineBy::Label,
            Some("seq") => BaselineBy::Seq,
            // Anything else — including an unrecognised value — counts as a
            // guess. Erring toward "this may be the wrong baseline" is the safe
            // direction: it invites verification instead of false confidence.
            _ => BaselineBy::Prefix,
        };
        Some(Self::build(
            want_canon,
            baseline_canon.as_deref()?,
            Fingerprints { want: want_fp.clone(), baseline: baseline_fp.clone()? },
            by,
        ))
    }

    /// Render at one fold depth. `artifact` is where the full record lives, so
    /// every folded view can point at the unabridged original.
    pub fn render(&self, fold: Fold, artifact: Option<&str>) -> String {
        let where_full = artifact.unwrap_or("<not written>");
        let guessed = if self.baseline_selected_by == BaselineBy::Prefix {
            "  (baseline picked by longest-prefix — A GUESS; verify it is the right step)"
        } else {
            ""
        };
        let head = format!(
            "request diff vs fp={} [{:?}]{guessed}\n  {} changed, {} added, {} removed; \
             {} identical bytes omitted\n  components: {}\n  full record: {where_full}\n",
            self.fingerprint.baseline,
            self.baseline_selected_by,
            self.summary.changed,
            self.summary.added,
            self.summary.removed,
            self.summary.identical_bytes,
            {
                let mut c: Vec<&str> = self
                    .changes
                    .iter()
                    .map(|c| c.component.as_deref().unwrap_or("(unnamed)"))
                    .collect();
                c.sort_unstable();
                c.dedup();
                c.join(", ")
            }
        );
        if fold == Fold::Summary {
            return head;
        }

        let mut out = head;
        for c in &self.changes {
            out.push_str(&format!(
                "\n{} {}  [{:?}]\n",
                c.component.as_deref().unwrap_or("(unnamed)"),
                c.path,
                c.op
            ));
            for (sign, v) in [("-", c.old.as_deref()), ("+", c.new.as_deref())] {
                let Some(v) = v else { continue };
                match fold {
                    Fold::Full => {
                        for l in v.split('\n') {
                            out.push_str(&format!("  {sign} {l}\n"));
                        }
                    }
                    _ => out.push_str(&fold_value(sign, v, &c.path, where_full)),
                }
            }
        }
        out
    }
}

/// Show a value's first and last lines and FOLD the middle, announcing how much
/// is hidden and where to read it. Not truncation: nothing is lost, and the
/// marker is a location.
fn fold_value(sign: &str, v: &str, path: &str, artifact: &str) -> String {
    const EDGE: usize = 3;
    let lines: Vec<&str> = v.split('\n').collect();
    if lines.len() <= EDGE * 2 + 1 {
        return lines.iter().map(|l| format!("  {sign} {l}\n")).collect();
    }
    let hidden = lines.len() - EDGE * 2;
    let chars: usize = lines[EDGE..lines.len() - EDGE].iter().map(|l| l.len()).sum();
    let mut s = String::new();
    for l in &lines[..EDGE] {
        s.push_str(&format!("  {sign} {l}\n"));
    }
    s.push_str(&format!(
        "  {sign} ⋯ folded {hidden} line(s) ({chars} chars) — full at {artifact}:{path} ⋯\n"
    ));
    for l in &lines[lines.len() - EDGE..] {
        s.push_str(&format!("  {sign} {l}\n"));
    }
    s
}

include!("cassette_diff_tests.rs");
