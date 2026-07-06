//! Incremental `ConverseStream` → canonical [`ChatEvent`] translation
//! (Doc 31 §6 C2 / M1). Pure, no-I/O: [`StreamTranslator`] folds the AWS
//! SDK's `ConverseStreamOutput` events into `tars-types` events one chunk
//! at a time, so the whole mapping is unit-testable without an AWS call —
//! the transport (`client::stream_response`) just drives it.
//!
//! This crate stays a leaf: it does **not** reuse `tars-provider`'s
//! `ToolCallBuffer`. The tool-use accumulation Bedrock needs (buffer the
//! partial `input` JSON fragments per content-block index, parse once at
//! block-stop) is small enough to keep local (Doc 31 §4).

use std::collections::HashMap;

use aws_sdk_bedrockruntime::types::{
    ContentBlockDelta, ContentBlockStart, ConverseStreamOutput, ReasoningContentBlockDelta,
};
use serde_json::Value;

use tars_types::{ChatEvent, ProviderError, StopReason, Usage};

use crate::mapping::{map_stop_reason, parse_usage};

/// Per-content-block accumulator for a streaming tool-use call. Mirrors
/// the `ToolCallBuffer` invariant (Doc 01 §8.1): **never** parse the args
/// mid-stream — concatenate the `input` fragments and parse once at
/// `ContentBlockStop`.
#[derive(Debug, Default)]
struct ToolAccum {
    id: String,
    /// Accumulated partial JSON `input` fragments.
    input: String,
}

/// Stateful, incremental translator for one `converse_stream()` response.
///
/// Bedrock's stream order is `messageStart`, then per content block
/// `contentBlockStart? · contentBlockDelta* · contentBlockStop`, then
/// `messageStop`, then `metadata` (Doc 31 §8.3). We surface `Started`
/// from the transport (not here) so this translator maps `MessageStart`
/// to nothing; the terminal `Finished` is emitted from [`Self::finish`]
/// at stream end, carrying the `stop_reason` seen on `messageStop` and the
/// `usage` seen on `metadata`.
#[derive(Debug, Default)]
pub struct StreamTranslator {
    /// Keyed by Bedrock's `content_block_index` (the same index rides
    /// `ContentBlockStart` / `ContentBlockDelta` / `ContentBlockStop`).
    tools: HashMap<i32, ToolAccum>,
    stop_reason: Option<StopReason>,
    usage: Option<Usage>,
    finished: bool,
}

impl StreamTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Translate one stream event into zero or more canonical events.
    /// A malformed tool-use `input` at block-stop is a real
    /// [`ProviderError::Parse`] carrying the raw fragment (CLAUDE.md #1),
    /// never a silent `{}` — an empty fragment (a no-arg tool call) *is*
    /// `{}` and is not an error.
    pub fn translate(
        &mut self,
        ev: &ConverseStreamOutput,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        Ok(match ev {
            // Transport emits `Started`; nothing to surface here.
            ConverseStreamOutput::MessageStart(_) => Vec::new(),

            ConverseStreamOutput::ContentBlockStart(e) => {
                // Only tool-use blocks carry a `start` payload; text /
                // reasoning blocks send `ContentBlockStart` with `None`.
                match e.start() {
                    Some(ContentBlockStart::ToolUse(tu)) => {
                        let cbi = e.content_block_index();
                        self.tools.insert(
                            cbi,
                            ToolAccum {
                                id: tu.tool_use_id().to_string(),
                                input: String::new(),
                            },
                        );
                        vec![ChatEvent::ToolCallStart {
                            index: slot(cbi),
                            id: tu.tool_use_id().to_string(),
                            name: tu.name().to_string(),
                        }]
                    }
                    _ => Vec::new(),
                }
            }

            ConverseStreamOutput::ContentBlockDelta(e) => {
                let cbi = e.content_block_index();
                match e.delta() {
                    Some(ContentBlockDelta::Text(t)) => vec![ChatEvent::Delta { text: t.clone() }],
                    // Only the visible reasoning text is a ThinkingDelta;
                    // signature / redacted-content chunks are opaque and have
                    // no canonical surface, so they fall through to nothing.
                    Some(ContentBlockDelta::ReasoningContent(ReasoningContentBlockDelta::Text(
                        t,
                    ))) => vec![ChatEvent::ThinkingDelta { text: t.clone() }],
                    Some(ContentBlockDelta::ToolUse(tu)) => {
                        // Buffer the partial JSON locally AND surface the
                        // fragment so a consumer can render progress; final
                        // parse happens at ContentBlockStop.
                        self.tools.entry(cbi).or_default().input.push_str(tu.input());
                        vec![ChatEvent::ToolCallArgsDelta {
                            index: slot(cbi),
                            args_delta: tu.input().to_string(),
                        }]
                    }
                    _ => Vec::new(),
                }
            }

            ConverseStreamOutput::ContentBlockStop(e) => {
                let cbi = e.content_block_index();
                match self.tools.remove(&cbi) {
                    // Tool-use block closed: parse the accumulated input once.
                    Some(accum) => {
                        let parsed_args = parse_tool_input(&accum.input, slot(cbi))?;
                        vec![ChatEvent::ToolCallEnd {
                            index: slot(cbi),
                            id: accum.id,
                            parsed_args,
                            thought_signature: None,
                        }]
                    }
                    // Text / reasoning block close — nothing to emit.
                    None => Vec::new(),
                }
            }

            ConverseStreamOutput::MessageStop(e) => {
                self.stop_reason = Some(map_stop_reason(e.stop_reason()));
                Vec::new()
            }

            ConverseStreamOutput::Metadata(e) => {
                if let Some(u) = e.usage() {
                    self.usage = Some(parse_usage(u));
                }
                Vec::new()
            }

            // #[non_exhaustive] SDK enum — an unknown future variant has no
            // canonical surface; drop it rather than guess.
            _ => Vec::new(),
        })
    }

    /// Emit the terminal [`ChatEvent::Finished`] exactly once, at stream
    /// end. A successful stream always terminates with Finished even if
    /// the provider never sent `messageStop` / `metadata` (defaulting the
    /// stop reason to `Other` and usage to zero) so the canonical contract
    /// (Finished emitted once) holds.
    pub fn finish(&mut self) -> Vec<ChatEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![ChatEvent::Finished {
            stop_reason: self.stop_reason.unwrap_or(StopReason::Other),
            usage: self.usage.unwrap_or_default(),
        }]
    }
}

/// Bedrock's `content_block_index` (`i32`, always ≥ 0) → the canonical
/// parallel-call slot (`usize`). A spec-impossible negative maps to 0.
fn slot(i: i32) -> usize {
    usize::try_from(i).unwrap_or(0)
}

/// Parse an accumulated tool-use `input` buffer into JSON. Empty → `{}`
/// (a no-arg call); a non-empty buffer that fails to parse is a real
/// [`ProviderError::Parse`] carrying the raw fragment (truncated), never
/// a fabricated value (CLAUDE.md #1).
fn parse_tool_input(raw: &str, index: usize) -> Result<Value, ProviderError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str::<Value>(trimmed).map_err(|e| {
        ProviderError::Parse(format!(
            "bedrock tool call (index {index}) args parse failed ({e}); raw: {}",
            truncate(raw, 200)
        ))
    })
}

/// Truncate to at most `max` bytes on a UTF-8 char boundary, appending `…`
/// when cut, so an error message never splits a multibyte char.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
        ConverseStreamMetadataEvent, MessageStartEvent, MessageStopEvent,
        StopReason as AwsStopReason, TokenUsage, ToolUseBlockDelta, ToolUseBlockStart,
    };
    use aws_sdk_bedrockruntime::types::ConversationRole;
    use serde_json::json;

    fn text_delta(idx: i32, text: &str) -> ConverseStreamOutput {
        ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(idx)
                .delta(ContentBlockDelta::Text(text.into()))
                .build()
                .unwrap(),
        )
    }

    fn thinking_delta(idx: i32, text: &str) -> ConverseStreamOutput {
        ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(idx)
                .delta(ContentBlockDelta::ReasoningContent(
                    ReasoningContentBlockDelta::Text(text.into()),
                ))
                .build()
                .unwrap(),
        )
    }

    fn tool_start(idx: i32, id: &str, name: &str) -> ConverseStreamOutput {
        ConverseStreamOutput::ContentBlockStart(
            ContentBlockStartEvent::builder()
                .content_block_index(idx)
                .start(ContentBlockStart::ToolUse(
                    ToolUseBlockStart::builder()
                        .tool_use_id(id)
                        .name(name)
                        .build()
                        .unwrap(),
                ))
                .build()
                .unwrap(),
        )
    }

    fn tool_input(idx: i32, frag: &str) -> ConverseStreamOutput {
        ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(idx)
                .delta(ContentBlockDelta::ToolUse(
                    ToolUseBlockDelta::builder().input(frag).build().unwrap(),
                ))
                .build()
                .unwrap(),
        )
    }

    fn block_stop(idx: i32) -> ConverseStreamOutput {
        ConverseStreamOutput::ContentBlockStop(
            ContentBlockStopEvent::builder()
                .content_block_index(idx)
                .build()
                .unwrap(),
        )
    }

    fn message_stop(reason: AwsStopReason) -> ConverseStreamOutput {
        ConverseStreamOutput::MessageStop(
            MessageStopEvent::builder().stop_reason(reason).build().unwrap(),
        )
    }

    fn metadata(input: i32, output: i32) -> ConverseStreamOutput {
        ConverseStreamOutput::Metadata(
            ConverseStreamMetadataEvent::builder()
                .usage(
                    TokenUsage::builder()
                        .input_tokens(input)
                        .output_tokens(output)
                        .total_tokens(input + output)
                        .build()
                        .unwrap(),
                )
                .build(),
        )
    }

    /// Drive a full event sequence through the translator (translate each,
    /// then finish) and collect the emitted canonical events.
    fn run(events: Vec<ConverseStreamOutput>) -> Vec<ChatEvent> {
        let mut t = StreamTranslator::new();
        let mut out = Vec::new();
        for e in &events {
            out.extend(t.translate(e).unwrap());
        }
        out.extend(t.finish());
        out
    }

    #[test]
    fn text_stream_maps_to_incremental_deltas_and_finish() {
        let events = vec![
            ConverseStreamOutput::MessageStart(
                MessageStartEvent::builder()
                    .role(ConversationRole::Assistant)
                    .build()
                    .unwrap(),
            ),
            text_delta(0, "Hel"),
            text_delta(0, "lo"),
            block_stop(0),
            message_stop(AwsStopReason::EndTurn),
            metadata(12, 3),
        ];
        let out = run(events);

        // MessageStart surfaces nothing; two text deltas, then Finished.
        assert_eq!(out.len(), 3, "events: {out:?}");
        assert!(matches!(&out[0], ChatEvent::Delta { text } if text == "Hel"));
        assert!(matches!(&out[1], ChatEvent::Delta { text } if text == "lo"));
        match &out[2] {
            ChatEvent::Finished { stop_reason, usage } => {
                assert_eq!(*stop_reason, StopReason::EndTurn);
                assert_eq!(usage.input_tokens, 12);
                assert_eq!(usage.output_tokens, 3);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_delta_maps_to_thinking() {
        let out = run(vec![
            thinking_delta(0, "let me think"),
            text_delta(1, "answer"),
            message_stop(AwsStopReason::EndTurn),
            metadata(1, 1),
        ]);
        assert!(matches!(&out[0], ChatEvent::ThinkingDelta { text } if text == "let me think"));
        assert!(matches!(&out[1], ChatEvent::Delta { text } if text == "answer"));
        assert!(matches!(&out[2], ChatEvent::Finished { .. }));
    }

    #[test]
    fn tool_use_accumulates_partial_json_across_deltas() {
        // The `input` arrives in fragments and must parse only at stop.
        let out = run(vec![
            tool_start(1, "call_1", "search"),
            tool_input(1, "{\"q\":"),
            tool_input(1, " \"rust"),
            tool_input(1, "\"}"),
            block_stop(1),
            message_stop(AwsStopReason::ToolUse),
            metadata(20, 8),
        ]);

        // Start, three arg deltas, End, Finished.
        assert!(matches!(
            &out[0],
            ChatEvent::ToolCallStart { index, id, name }
                if *index == 1 && id == "call_1" && name == "search"
        ));
        assert!(matches!(&out[1], ChatEvent::ToolCallArgsDelta { index, .. } if *index == 1));
        match &out[4] {
            ChatEvent::ToolCallEnd {
                index,
                id,
                parsed_args,
                ..
            } => {
                assert_eq!(*index, 1);
                assert_eq!(id, "call_1");
                assert_eq!(*parsed_args, json!({ "q": "rust" }));
            }
            other => panic!("expected ToolCallEnd, got {other:?}"),
        }
        match &out[5] {
            ChatEvent::Finished { stop_reason, usage } => {
                assert_eq!(*stop_reason, StopReason::ToolUse);
                assert_eq!(usage.output_tokens, 8);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn empty_tool_input_is_empty_object_not_error() {
        let out = run(vec![
            tool_start(0, "c", "noargs"),
            block_stop(0),
            message_stop(AwsStopReason::ToolUse),
        ]);
        match &out[1] {
            ChatEvent::ToolCallEnd { parsed_args, .. } => {
                assert_eq!(*parsed_args, json!({}));
            }
            other => panic!("expected ToolCallEnd, got {other:?}"),
        }
    }

    #[test]
    fn malformed_tool_input_is_honest_parse_error_with_raw() {
        let mut t = StreamTranslator::new();
        t.translate(&tool_start(0, "c", "t")).unwrap();
        t.translate(&tool_input(0, "{not json")).unwrap();
        let err = t.translate(&block_stop(0)).unwrap_err();
        match err {
            ProviderError::Parse(m) => {
                assert!(m.contains("not json"), "raw fragment must survive: {m}");
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn finish_emits_exactly_once() {
        let mut t = StreamTranslator::new();
        assert_eq!(t.finish().len(), 1);
        assert_eq!(t.finish().len(), 0, "Finished must not be re-emitted");
    }
}
