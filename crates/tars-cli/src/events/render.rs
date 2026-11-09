//! Renders what `tars events` read as the text it prints: the call table of `list`, and the
//! bodies and wire bytes of `show`.

use serde::Serialize;
use tars_melt::event::call_log::{CallBody, LlmCallRow};
use tars_melt::event::wire::{Transport, WireRow};
use tars_types::Millis;

use super::{CallEndView, CallSummaryView, wire_outcome};

pub(super) fn render_list(lines: &[CallSummaryView]) -> String {
    if lines.is_empty() {
        return "(no calls)\n".to_owned();
    }
    let header = format!(
        "{:<36}  {:<19}  {:<14}  {:<28}  {:<12}  tags\n{}\n",
        "call_id",
        "at",
        "tenant",
        "model",
        "outcome",
        "-".repeat(120)
    );
    lines.iter().fold(header, |mut out, l| {
        out.push_str(&format!(
            "{:<36}  {:<19}  {:<14}  {:<28}  {:<12}  {}\n",
            l.call_id,
            format_ms(l.at_ms),
            truncate(&l.tenant, 14),
            truncate(&l.model, 28),
            l.outcome.to_string(),
            l.tags.join(","),
        ));
        out
    })
}

/// What the text of `show` says when a call has no wire row.
const NO_WIRE: &str = "这次调用没有 wire 记录";

/// Renders each attempt's bytes as sent and received.
pub(super) fn render_wire(attempts: &[WireRow]) -> String {
    if attempts.is_empty() {
        return format!("\n=== WIRE ===\n({NO_WIRE})\n");
    }
    let mut out = String::new();
    for a in attempts {
        let (outcome, _) = wire_outcome(&a.outcome);
        let (status, process) = match &a.transport {
            Transport::HttpSse { status } => (*status, None),
            Transport::Subprocess { process } => (None, process.as_ref()),
        };
        let status = status.map_or_else(|| "no status".to_owned(), |s| format!("HTTP {s}"));
        let cut = if a.response_truncated {
            ", cut at the transport's cap"
        } else {
            ""
        };
        // A delegate's request is its stdin; its argv is said apart.
        let sent = match a.transport {
            Transport::HttpSse { .. } => "REQUEST BYTES",
            Transport::Subprocess { .. } => "STDIN BYTES",
        };
        out.push_str(&format!(
            "\n=== WIRE ATTEMPT {n} ({outcome}, {status}) {sent} ===\n{req}\n",
            n = a.attempt,
            req = raw_text(&a.request_bytes),
        ));
        if let Some(argv) = process.map(|p| &p.argv) {
            out.push_str(&format!(
                "=== WIRE ATTEMPT {n} ARGV ===\n{argv:?}\n",
                n = a.attempt,
            ));
        }
        out.push_str(&format!(
            "=== WIRE ATTEMPT {n} RESPONSE BYTES{cut} ===\n{resp}\n",
            n = a.attempt,
            resp = raw_text(&a.response_bytes),
        ));
        if let Some(stderr) = process.map(|p| &p.stderr) {
            out.push_str(&format!(
                "=== WIRE ATTEMPT {n} STDERR BYTES ===\n{err}\n",
                n = a.attempt,
                err = raw_text(stderr),
            ));
        }
    }
    out
}

/// Returns the bytes as text; bytes that are not UTF-8 are labelled and shown with U+FFFD.
fn raw_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => format!(
            "({} bytes, not UTF-8; invalid bytes shown as U+FFFD)\n{}",
            bytes.len(),
            String::from_utf8_lossy(bytes)
        ),
    }
}

pub(super) fn render_bodies(row: &LlmCallRow) -> Result<String, serde_json::Error> {
    let request = render_body(&row.request_body)?;
    let response = match row.response() {
        Some(r) => render_body(&r.body)?,
        None => format!(
            "(no response: the call ended {})\n",
            CallEndView::from(&row.outcome())
        ),
    };
    Ok(format!(
        "\n=== REQUEST BODY ===\n{request}\n=== RESPONSE BODY ===\n{response}"
    ))
}

/// Renders a JSON body pretty-printed and any other text as kept; a withheld body is said so.
fn render_body(body: &CallBody) -> Result<String, serde_json::Error> {
    Ok(match body {
        CallBody::Present(text) => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(v) => format!("{}\n", serde_json::to_string_pretty(&v)?),
            Err(_) => format!("{text}\n"),
        },
        CallBody::Withheld => "(withheld: the writer's policy keeps no bodies)\n".to_owned(),
        CallBody::Unserializable { error } => format!("(did not serialize: {error})\n"),
    })
}

pub(super) fn json_lines<T: Serialize>(items: &[T]) -> Result<String, serde_json::Error> {
    items.iter().try_fold(String::new(), |mut out, item| {
        out.push_str(&serde_json::to_string(item)?);
        out.push('\n');
        Ok(out)
    })
}

fn format_ms(ms: Millis) -> String {
    match chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        // Out of chrono's range: make the corruption visible instead of
        // collapsing it to a plausible-looking default.
        None => format!("<invalid:at_ms={ms}>"),
    }
}

fn truncate(s: &str, n: usize) -> String {
    // Cut by character: a byte slice can split a code point and panic.
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let keep = n.saturating_sub(1);
        let end = s.char_indices().nth(keep).map_or(s.len(), |(i, _)| i);
        format!("{}…", &s[..end])
    }
}
