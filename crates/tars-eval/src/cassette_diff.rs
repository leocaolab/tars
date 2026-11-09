//! Says, before a test sends a request to a replaying cassette, whether the cassette recorded it,
//! and what changed when it did not. The cassette itself only looks a fingerprint up; whether a
//! difference matters is the test's decision. Design: `docs/design/cassette-request-diff.md`.
//!
//! Two rules run through everything here:
//!   - **Omit what is identical, never truncate what differs.** The first is the
//!     definition of a diff; the second would throw away the record's only
//!     content.
//!   - **Fold for display, never for storage.** Folding announces its size and
//!     leaves the original reachable; truncation is silent and irreversible.

use std::collections::BTreeMap;

use serde_json::Value;
use tars_llm::ChatRequest;
use tars_llm::max_output_tokens_sent;
use tars_llm::provider::backends::cassette::{CassetteFile, canon_request, request_fingerprint};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Changed,
}

/// One located difference. `old`/`new` carry the FULL value — folding happens
/// only when rendering.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RequestChange {
    /// JSON Pointer (RFC 6901). Survives reformatting, unlike a line number,
    /// and lets a tool jump straight to the value.
    pub path: String,
    /// The request part this path lands in, when tars can name it. `None` when
    /// it cannot — a guessed component name is a fabricated clue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    pub op: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiffCounts {
    pub changed: usize,
    pub added: usize,
    pub removed: usize,
    /// Length in bytes of the prefix the two request bodies share, printed so the reader knows
    /// this is a diff, not a snapshot.
    pub identical_bytes: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RequestDiff {
    pub kind: &'static str,
    pub version: u32,
    pub changes: Vec<RequestChange>,
    pub summary: DiffCounts,
}

/// Why a request will not replay from its cassette.
///
/// When the request differs from the recording the test expected, the variant names the outermost
/// part that changed (the system prompt, then the context, then the parameters), and the diff
/// carries every change.
#[derive(Debug, thiserror::Error)]
pub enum CassetteDiffError {
    /// The request is not recorded, and neither is the recording `expected` it was to match.
    #[error(
        "request fp={fingerprint} is not recorded, and the cassette has no recording {expected} to compare it with"
    )]
    Miss {
        fingerprint: String,
        expected: String,
    },
    #[error("the system prompt differs from the recording\n{}", .0.render(FoldDepth::Summary, None))]
    Prompt(RequestDiff),
    /// The messages or the tool specs differ.
    #[error("the context differs from the recording\n{}", .0.render(FoldDepth::Summary, None))]
    Context(RequestDiff),
    /// Anything else differs: the model, sampling, the output limit, the schema.
    #[error("the parameters differ from the recording\n{}", .0.render(FoldDepth::Summary, None))]
    Params(RequestDiff),
}

/// The request part a difference lands in, ordered outermost first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    Prompt,
    Context,
    Params,
}

fn part_of(path: &str) -> Part {
    match path.split('/').nth(1) {
        Some("system") => Part::Prompt,
        Some("messages" | "tools") => Part::Context,
        _ => Part::Params,
    }
}

/// Checks `req`, as the service will send it to `model`, against the cassette `file` before the
/// call. A recorded request is `Ok`; otherwise it is compared with the recording under
/// `expected`, the key of the recording the test means this request to match.
///
/// # Errors
///
/// Returns [`CassetteDiffError::Miss`] when neither the request nor `expected` is recorded, and
/// otherwise the variant naming the outermost part that changed.
pub fn compare(
    file: &CassetteFile,
    req: &ChatRequest,
    model: &str,
    expected: &str,
) -> Result<(), CassetteDiffError> {
    let mut sent = req.clone();
    sent.max_output_tokens =
        max_output_tokens_sent(req.max_output_tokens, &file.output_limit(model));
    let fingerprint = request_fingerprint(&sent, model);
    if file.recordings().contains_key(&fingerprint) {
        return Ok(());
    }

    let Some(recording) = file.recordings().get(expected) else {
        return Err(CassetteDiffError::Miss {
            fingerprint,
            expected: expected.to_string(),
        });
    };
    let diff = RequestDiff::build(&canon_request(&sent, model), &recording.request);
    let part = diff.changes.iter().map(|c| part_of(&c.path)).min().expect(
        "a cassette file holds each recording under its request's fingerprint and this request's \
         fingerprint is not a key, so the two canonical texts differ and the diff has a change",
    );
    Err(match part {
        Part::Prompt => CassetteDiffError::Prompt(diff),
        Part::Context => CassetteDiffError::Context(diff),
        Part::Params => CassetteDiffError::Params(diff),
    })
}

/// Display depth. Storage is always `Full`; these only shape a view. A [`CassetteDiffError`] shows
/// itself at `Summary`; a caller that keeps the full record somewhere renders the deeper views
/// with its location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldDepth {
    /// One line: counts + components + where the full record is.
    Summary,
    /// Each change located, values folded to their first/last lines.
    Folded,
    /// Every changed value in full.
    Full,
}

/// Names the request part a pointer lands in. Only the shapes tars owns (`ChatRequest`) are
/// named; anything else yields `None` rather than a guess.
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

/// Which of the two compared values hold a location.
#[derive(Clone, Copy)]
enum Sides<'a> {
    Both(&'a Value, &'a Value),
    Old(&'a Value),
    New(&'a Value),
}

/// Pairs the keys of two objects in key order, each with the side or sides that hold it.
fn sides<'a>(
    old: &'a serde_json::Map<String, Value>,
    new: &'a serde_json::Map<String, Value>,
) -> BTreeMap<&'a String, Sides<'a>> {
    let mut keys: BTreeMap<&String, Sides> = old
        .iter()
        .map(|(k, o)| {
            let side = match new.get(k) {
                Some(n) => Sides::Both(o, n),
                None => Sides::Old(o),
            };
            (k, side)
        })
        .collect();
    keys.extend(
        new.iter()
            .filter(|(k, _)| !old.contains_key(*k))
            .map(|(k, n)| (k, Sides::New(n))),
    );
    keys
}

/// Returns the change at `path`: changed where both sides hold it, removed where only the old
/// one does, added where only the new one does.
fn change_at(path: String, side: Sides) -> RequestChange {
    let (op, old, new) = match side {
        Sides::Both(o, n) => (ChangeKind::Changed, Some(o), Some(n)),
        Sides::Old(o) => (ChangeKind::Removed, Some(o), None),
        Sides::New(n) => (ChangeKind::Added, None, Some(n)),
    };
    RequestChange {
        component: component_of(&path).map(str::to_string),
        path,
        op,
        old: old.map(repr),
        new: new.map(repr),
    }
}

/// Walks both values and emits one [`RequestChange`] per differing location; identical subtrees
/// emit nothing.
fn walk(a: &Value, b: &Value, path: &str, out: &mut Vec<RequestChange>) {
    if a == b {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, side) in sides(x, y) {
                let p = format!("{path}/{}", esc(k));
                match side {
                    Sides::Both(av, bv) => walk(av, bv, &p, out),
                    one_side => out.push(change_at(p, one_side)),
                }
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            let shared = x.len().min(y.len());
            for (i, (av, bv)) in x.iter().zip(y).enumerate() {
                walk(av, bv, &format!("{path}/{i}"), out);
            }
            for (i, av) in x.iter().enumerate().skip(shared) {
                out.push(change_at(format!("{path}/{i}"), Sides::Old(av)));
            }
            for (i, bv) in y.iter().enumerate().skip(shared) {
                out.push(change_at(format!("{path}/{i}"), Sides::New(bv)));
            }
        }
        _ => out.push(change_at(path.to_string(), Sides::Both(a, b))),
    }
}

impl RequestDiff {
    /// Diffs two canonical request texts, each `model=<m>\0<json>`. The JSON half is compared
    /// structurally so a change reports where it landed; when either side is not JSON, or the
    /// two parse equal but their bytes differ, the whole body is one change. Texts that differ
    /// in a way neither half shows (one lacks the `model=` prefix) are one change of the whole
    /// text, so a diff is empty exactly when the texts are equal.
    pub fn build(want: &str, baseline: &str) -> Self {
        let split = |s: &str| match s.split_once('\0') {
            Some((m, body)) => (
                m.strip_prefix("model=").unwrap_or(m).to_string(),
                body.to_string(),
            ),
            None => (String::new(), s.to_string()),
        };
        let (wm, wb) = split(want);
        let (bm, bb) = split(baseline);

        let mut changes = Vec::new();
        if wm != bm {
            changes.push(RequestChange {
                path: "/model".into(),
                component: Some("model".into()),
                op: ChangeKind::Changed,
                old: Some(bm),
                new: Some(wm),
            });
        }
        match (
            serde_json::from_str::<Value>(&bb),
            serde_json::from_str::<Value>(&wb),
        ) {
            (Ok(a), Ok(b)) if a != b => walk(&a, &b, "", &mut changes),
            _ if bb != wb => changes.push(RequestChange {
                path: "/".into(),
                component: None,
                op: ChangeKind::Changed,
                old: Some(bb.clone()),
                new: Some(wb.clone()),
            }),
            _ => {}
        }
        if changes.is_empty() && want != baseline {
            changes.push(RequestChange {
                path: String::new(),
                component: None,
                op: ChangeKind::Changed,
                old: Some(baseline.to_string()),
                new: Some(want.to_string()),
            });
        }

        let identical_bytes = bb
            .bytes()
            .zip(wb.bytes())
            .take_while(|(x, y)| x == y)
            .count();
        let summary = DiffCounts {
            changed: changes
                .iter()
                .filter(|c| c.op == ChangeKind::Changed)
                .count(),
            added: changes.iter().filter(|c| c.op == ChangeKind::Added).count(),
            removed: changes
                .iter()
                .filter(|c| c.op == ChangeKind::Removed)
                .count(),
            identical_bytes,
        };
        Self {
            kind: "cassette-request-diff",
            version: 1,
            changes,
            summary,
        }
    }

    /// Returns the names of the request parts that changed, sorted and each once.
    fn components(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .changes
            .iter()
            .map(|c| c.component.as_deref().unwrap_or("(unnamed)"))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Renders the diff at one fold depth. `artifact` is where the full record lives, so every
    /// folded view can point at it; without one, a folded value points at its path in the diff.
    pub fn render(&self, fold: FoldDepth, artifact: Option<&str>) -> String {
        let record = artifact.map_or(String::new(), |a| format!("  full record: {a}\n"));
        let head = format!(
            "request diff vs the recording\n  {} changed, {} added, {} removed; \
             {} leading bytes identical\n  components: {}\n{record}",
            self.summary.changed,
            self.summary.added,
            self.summary.removed,
            self.summary.identical_bytes,
            self.components().join(", ")
        );
        if fold == FoldDepth::Summary {
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
                    FoldDepth::Full => {
                        for l in v.split('\n') {
                            out.push_str(&format!("  {sign} {l}\n"));
                        }
                    }
                    _ => out.push_str(&fold_value(sign, v, &c.path, artifact)),
                }
            }
        }
        out
    }
}

/// Shows a value's first and last three lines and folds the middle, saying how much is hidden and
/// where to read it; nothing is lost, and the marker is a location.
fn fold_value(sign: &str, v: &str, path: &str, artifact: Option<&str>) -> String {
    const EDGE: usize = 3;
    let lines: Vec<&str> = v.split('\n').collect();
    if lines.len() <= EDGE * 2 + 1 {
        return lines.iter().map(|l| format!("  {sign} {l}\n")).collect();
    }
    let hidden = lines.len() - EDGE * 2;
    let chars: usize = lines[EDGE..lines.len() - EDGE]
        .iter()
        .map(|l| l.chars().count())
        .sum();
    let at = artifact.map_or(path.to_string(), |a| format!("{a}:{path}"));
    let mut s = String::new();
    for l in &lines[..EDGE] {
        s.push_str(&format!("  {sign} {l}\n"));
    }
    s.push_str(&format!(
        "  {sign} ⋯ folded {hidden} line(s) ({chars} chars) — full at {at} ⋯\n"
    ));
    for l in &lines[lines.len() - EDGE..] {
        s.push_str(&format!("  {sign} {l}\n"));
    }
    s
}

#[cfg(test)]
#[path = "cassette_diff_tests.rs"]
mod tests;
