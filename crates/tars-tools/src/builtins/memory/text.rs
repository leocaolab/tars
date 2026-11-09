//! The pure half of the memory tool: text in, text out. Rendering follows the API
//! doc's return shapes; where the doc is silent, the Python SDK's
//! `BetaLocalFilesystemMemoryTool`.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use tars_types::SessionId;

use super::error::MemoryError;

/// API doc: "Files with more than 999,999 lines should return an error".
const MAX_LINES: usize = 999_999;
/// API doc: line numbers are 6 characters, right-aligned.
const LINE_NUMBER_WIDTH: usize = 6;
/// SDK: the `str_replace` snippet shows this many lines either side of the change.
const SNIPPET_CONTEXT: usize = 2;

/// Claude Code loads the first 200 lines or 25KB of `MEMORY.md`, whichever comes first.
pub(super) const INDEX_FILE: &str = "MEMORY.md";
const INDEX_MAX_LINES: usize = 200;
const INDEX_MAX_BYTES: usize = 25 * 1024;

/// The lines a `view` shows, as the API sends them: `[start, end]`, 1-based and inclusive, with
/// `-1` for the end of the file.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(from = "[i64; 2]")]
pub(super) struct ViewRange {
    start: i64,
    end: ViewEnd,
}

#[derive(Clone, Copy, Debug)]
enum ViewEnd {
    Line(i64),
    EndOfFile,
}

impl From<[i64; 2]> for ViewRange {
    fn from([start, end]: [i64; 2]) -> Self {
        let end = if end == -1 {
            ViewEnd::EndOfFile
        } else {
            ViewEnd::Line(end)
        };
        Self { start, end }
    }
}

pub(super) fn render_file(
    path: &str,
    text: &str,
    view_range: Option<ViewRange>,
) -> Result<String, MemoryError> {
    let lines: Vec<&str> = text
        .strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect();
    if lines.len() > MAX_LINES {
        return Err(MemoryError::TooManyLines(path.to_string()));
    }
    let (start, end) = match view_range {
        Some(ViewRange { start, end }) => (
            clamp_line(start.max(1) - 1, lines.len()),
            end_of(end, lines.len()),
        ),
        None => (0, lines.len()),
    };
    let shown = lines.get(start..end.max(start)).unwrap_or(&[]);
    Ok(format!(
        "Here's the content of {path} with line numbers:\n{}",
        numbered(shown, start + 1)
    ))
}

fn end_of(end: ViewEnd, len: usize) -> usize {
    match end {
        ViewEnd::Line(n) => clamp_line(n, len),
        ViewEnd::EndOfFile => len,
    }
}

fn clamp_line(n: i64, len: usize) -> usize {
    usize::try_from(n).map_or(0, |n| n.min(len))
}

fn numbered(lines: &[&str], first: usize) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>LINE_NUMBER_WIDTH$}\t{line}", first + i))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One row of a directory listing: its size and its path relative to the listed dir.
pub(super) struct ListingEntry {
    pub size: u64,
    pub relative: String,
    pub is_dir: bool,
}

pub(super) fn render_listing(path: &str, dir_size: u64, entries: &[ListingEntry]) -> String {
    let header = format!(
        "Here're the files and directories up to 2 levels deep in {path}, excluding hidden items and node_modules:"
    );
    let rows = std::iter::once(format!("{}\t{path}", human_size(dir_size))).chain(
        entries.iter().map(|e| {
            let slash = if e.is_dir { "/" } else { "" };
            format!("{}\t{path}/{}{slash}", human_size(e.size), e.relative)
        }),
    );
    std::iter::once(header)
        .chain(rows)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Formats a size as the SDK's `_format_file_size` does: B/K/M/G, one decimal unless whole.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    if bytes == 0 {
        return "0B".to_string();
    }
    let exp = ((63 - bytes.leading_zeros()) / 10).min(3);
    let size = bytes as f64 / 1024_f64.powi(exp as i32);
    let unit = UNITS[exp as usize];
    if size.fract() == 0.0 {
        format!("{size:.0}{unit}")
    } else {
        format!("{size:.1}{unit}")
    }
}

/// The edited text and the 0-based line the edit starts on.
pub(super) struct Replacement {
    pub text: String,
    pub line: usize,
}

pub(super) fn replace_once(
    path: &str,
    text: &str,
    old_str: &str,
    new_str: &str,
) -> Result<Replacement, MemoryError> {
    let hits: Vec<usize> = text.match_indices(old_str).map(|(at, _)| at).collect();
    match hits.as_slice() {
        [] => Err(MemoryError::NoMatch {
            path: path.to_string(),
            old_str: old_str.to_string(),
        }),
        [at] => Ok(Replacement {
            text: text.replacen(old_str, new_str, 1),
            line: line_of(text, *at),
        }),
        many => Err(MemoryError::Ambiguous {
            old_str: old_str.to_string(),
            lines: many.iter().map(|at| line_of(text, *at) + 1).collect(),
        }),
    }
}

fn line_of(text: &str, byte: usize) -> usize {
    text[..byte].matches('\n').count()
}

pub(super) fn replace_snippet(text: &str, line: usize) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let start = line.saturating_sub(SNIPPET_CONTEXT);
    let end = (line + SNIPPET_CONTEXT + 1).min(lines.len());
    format!(
        "The memory file has been edited. Here is the snippet showing the change (with line numbers):\n{}",
        numbered(lines.get(start..end).unwrap_or(&[]), start + 1)
    )
}

/// Inserts `insert_text` after line `insert_line` as the SDK's `insert` does: it splits on LF
/// without the empty tail and ends the file with a newline.
pub(super) fn insert_at(
    text: &str,
    insert_line: i64,
    insert_text: &str,
) -> Result<String, MemoryError> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let n_lines = lines.len();
    let at = usize::try_from(insert_line)
        .ok()
        .filter(|at| *at <= n_lines)
        .ok_or(MemoryError::InsertLine {
            insert_line,
            n_lines,
        })?;
    lines.insert(at, insert_text.trim_end_matches('\n'));
    Ok(format!("{}\n", lines.join("\n")))
}

/// What the handler stamps into a memory file's frontmatter on every write.
pub(super) struct Stamp<'a> {
    pub modified: DateTime<Utc>,
    pub origin_session_id: &'a SessionId,
}

const STAMPED_KEYS: [&str; 2] = ["modified:", "origin_session_id:"];

/// Stamps `modified` and `origin_session_id` into the file's YAML frontmatter, replacing earlier
/// values. A file without frontmatter is left exactly as written: like Claude Code, the handler
/// never adds frontmatter.
pub(super) fn stamp(text: &str, stamp: &Stamp<'_>) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some(close) = frontmatter_close(&lines) else {
        return text.to_string();
    };
    let kept = lines[1..close]
        .iter()
        .filter(|l| !STAMPED_KEYS.iter().any(|k| l.starts_with(k)));
    let stamped = [
        format!(
            "modified: {}\n",
            stamp.modified.to_rfc3339_opts(SecondsFormat::Millis, true)
        ),
        format!("origin_session_id: {}\n", stamp.origin_session_id),
    ];
    std::iter::once(lines[0].to_string())
        .chain(kept.map(|l| l.to_string()))
        .chain(stamped)
        .chain(lines[close..].iter().map(|l| l.to_string()))
        .collect()
}

/// Returns the index of the closing `---` line, when the text opens with a `---` line.
fn frontmatter_close(lines: &[&str]) -> Option<usize> {
    let is_fence = |l: &str| l.trim_end() == "---";
    lines.first().filter(|l| is_fence(l))?;
    lines
        .iter()
        .skip(1)
        .position(|l| is_fence(l))
        .map(|i| i + 1)
}

/// Returns where line `line` of `before` sits in `after`: moved by the lines stamping added or
/// removed when it is at or past the frontmatter's closing fence, unchanged above it.
pub(super) fn stamped_line(before: &str, after: &str, line: usize) -> usize {
    let lines: Vec<&str> = before.split_inclusive('\n').collect();
    match frontmatter_close(&lines) {
        Some(close) if line >= close => {
            let delta =
                after.matches('\n').count() as isize - before.matches('\n').count() as isize;
            line.saturating_add_signed(delta)
        }
        _ => line,
    }
}

/// Returns the part of `MEMORY.md` a session start loads, cut as Claude Code cuts it: trimmed,
/// the first 200 lines, then back to the last whole line inside 25KB. A blank index loads
/// nothing, so the caller shows no index section at all.
pub(super) fn loaded_index(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let by_lines = match trimmed.match_indices('\n').nth(INDEX_MAX_LINES - 1) {
        Some((end, _)) => &trimmed[..end],
        None => trimmed,
    };
    let loaded = if by_lines.len() <= INDEX_MAX_BYTES {
        by_lines
    } else {
        let head = &by_lines[..char_floor(by_lines, INDEX_MAX_BYTES)];
        head.rfind('\n').map_or(head, |end| &head[..end])
    };
    (!loaded.is_empty()).then_some(loaded)
}

/// Returns the last char boundary at or before `at`; 0 always is one.
fn char_floor(text: &str, at: usize) -> usize {
    (1..=at)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0)
}

/// Says, in Claude Code's words, what was cut when `loaded` (from [`loaded_index`]) is less
/// than all of `text`; the caller shows it as `> WARNING: …` under the loaded index.
/// `None` when nothing was cut.
pub(super) fn truncation_warning(text: &str, loaded: &str) -> Option<String> {
    let trimmed = text.trim();
    if loaded.len() == trimmed.len() {
        return None;
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let total = lines.len();
    let ends_on_a_line = trimmed[loaded.len()..].starts_with('\n');
    let whole = loaded.lines().count() - usize::from(!ends_on_a_line);
    let size = trimmed.len();
    let what = match (total > INDEX_MAX_LINES, size > INDEX_MAX_BYTES) {
        (false, true) => format!(
            "{} (limit: {}) \u{2014} index entries are too long",
            kilobytes(size),
            kilobytes(INDEX_MAX_BYTES)
        ),
        (true, false) => format!("{total} lines (limit: {INDEX_MAX_LINES})"),
        _ => format!("{total} lines and {}", kilobytes(size)),
    };
    let cut = if whole == 0 {
        format!(
            "everything after the first {} characters of line 1 was cut off",
            loaded.chars().count()
        )
    } else {
        let next = lines[whole].trim();
        let quote = if next.is_empty() {
            String::new()
        } else {
            format!(" (\"{}\")", clip(next, 80))
        };
        format!(
            "{} of {total} lines were cut off, starting at line {}{quote}",
            total - whole,
            whole + 1
        )
    };
    Some(format!(
        "{INDEX_FILE} is {what}. Only part of it was loaded: {cut}. \
         Keep index entries to one line under ~200 chars; move detail into topic files."
    ))
}

fn kilobytes(bytes: usize) -> String {
    let kb = bytes as f64 / 1024.0;
    if kb.fract() == 0.0 {
        format!("{kb:.0}KB")
    } else {
        format!("{kb:.1}KB")
    }
}

/// Returns the first `max` characters, with `…` when something was dropped.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}\u{2026}", &text[..end]),
        None => text.to_string(),
    }
}

/// `MEMORY.md` is past what the next session will load: which limit, and by how much.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum IndexOverflow {
    Lines(usize),
    Bytes(usize),
}

pub(super) fn index_overflow(text: &str) -> Option<IndexOverflow> {
    let lines = text.lines().count();
    if lines > INDEX_MAX_LINES {
        Some(IndexOverflow::Lines(lines))
    } else if text.len() > INDEX_MAX_BYTES {
        Some(IndexOverflow::Bytes(text.len()))
    } else {
        None
    }
}

/// Returns the reply for a write that overflowed the index, in Claude Code's shape ("this write
/// left the memory index at MEMORY.md at …, over its … read limit") with its advice for
/// shortening the index.
pub(super) fn index_overflow_message(done: &str, overflow: &IndexOverflow) -> String {
    let (at, limit) = match overflow {
        IndexOverflow::Lines(n) => (format!("{n} lines"), format!("{INDEX_MAX_LINES}-line")),
        IndexOverflow::Bytes(n) => (format!("{n} bytes"), format!("{INDEX_MAX_BYTES}-byte")),
    };
    format!(
        "{done}\nError: this write left the memory index at {INDEX_FILE} at {at}, over its {limit} read limit. \
         The write succeeded, but everything past the limit is dropped on the next load. \
         Rewrite the index now: keep one line per entry, move detail into topic files, \
         and merge or drop stale entries."
    )
}
