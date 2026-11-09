//! Accumulates the argument fragments of streamed tool calls and parses each call's arguments
//! once, when the call ends; a fragment is never parsed on its own.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use tars_types::{
    ArgumentText, ChatEvent, MalformedArguments, StopReason, ToolCall, Usage, parse_arguments,
};

use crate::provider::error::ParseProblem;
use crate::{ProviderError, ReceivedToolCall};

#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    name: String,
    args: String,
}

/// The per-stream state of an HTTP adapter's event parser: tool calls in flight, keyed by
/// `index` because OpenAI and Anthropic interleave parallel calls, and the few stream-level facts
/// a stateless `parse_event` needs (whether `Started` and `Finished` went out, a stop reason
/// waiting for its usage).
#[derive(Debug, Default)]
pub struct StreamDecodeState {
    inflight: BTreeMap<usize, PendingCall>,
    started_emitted: bool,
    finished_emitted: bool,
    terminator_seen: bool,
    /// How many calls this stream has finalized; Gemini numbers its calls by it, since one
    /// response can spread its calls over several events.
    ended_calls: usize,
    /// The stop reason of a `finish_reason` chunk that carried no usage, kept for the usage-only
    /// chunk after it; emitting `EndTurn` there would hide a `ToolUse` or `MaxTokens`.
    pending_stop_reason: Option<StopReason>,
    /// The stop reason and usage of the last intermediate finish payload (Anthropic's
    /// `message_delta`), kept for the `Finished` a bare terminator (`message_stop`) sends and for
    /// judging a call whose block closes after the stop reason came.
    last_stop_reason: Option<StopReason>,
    last_usage: Option<Usage>,
    /// The first call whose arguments did not parse when its block closed, held until the turn's
    /// stop reason says whether the output was cut off.
    held: Option<UnparsedToolCall>,
}

impl StreamDecodeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keeps the stop reason of a chunk without usage for the usage-only chunk that follows.
    pub fn record_pending_stop(&mut self, reason: StopReason) {
        self.pending_stop_reason = Some(reason);
    }

    pub fn take_pending_stop(&mut self) -> Option<StopReason> {
        self.pending_stop_reason.take()
    }

    /// Returns `true` on the first call and `false` after, so an adapter emits `Started` once.
    pub fn take_started(&mut self) -> bool {
        if self.started_emitted {
            false
        } else {
            self.started_emitted = true;
            true
        }
    }

    /// Keeps whichever of a stop reason and a usage an intermediate finish payload carried; a
    /// later payload replaces only what it carries.
    pub fn record_finish_state(&mut self, stop_reason: Option<StopReason>, usage: Option<Usage>) {
        if stop_reason.is_some() {
            self.last_stop_reason = stop_reason;
        }
        if usage.is_some() {
            self.last_usage = usage;
        }
    }

    /// Returns the last event the kept stop reason and usage make ([`ChatEvent::stream_end`]):
    /// `Finished` when a payload said why the turn stopped, `UsageProgress` when payloads only
    /// reported usage, and none when they said neither.
    pub fn recorded_end(&self) -> Option<ChatEvent> {
        ChatEvent::stream_end(self.last_stop_reason, self.last_usage)
    }

    /// Records that `Finished` went out, so a later terminator does not emit a second one.
    pub fn mark_finished(&mut self) {
        self.finished_emitted = true;
    }

    pub fn finished_emitted(&self) -> bool {
        self.finished_emitted
    }

    /// Returns whether a tool call is in flight at `index`. A block stop at such an index must
    /// finalize, or the consumer waits forever for its `ToolCallEnd`; at any other index it closed
    /// a text or thinking block.
    pub fn is_inflight(&self, index: usize) -> bool {
        self.inflight.contains_key(&index)
    }

    /// Returns how many calls this stream has finalized.
    pub fn ended_calls(&self) -> usize {
        self.ended_calls
    }

    /// Returns the indexes of the calls in flight, lowest first.
    pub fn inflight_indexes(&self) -> Vec<usize> {
        self.inflight.keys().copied().collect()
    }

    pub fn on_start(&mut self, index: usize, id: String, name: String) {
        // Keeps any arguments already received: some providers send deltas before the start, and
        // a duplicate start is logged, not allowed to erase them.
        if let Some(entry) = self.inflight.get(&index)
            && (!entry.id.is_empty() || !entry.name.is_empty())
        {
            tracing::warn!(
                index,
                old_id = %entry.id,
                new_id = %id,
                "duplicate ToolCallStart for index; preserving accumulated args",
            );
        }
        let entry = self.inflight.entry(index).or_default();
        // A partial start chunk with an empty id or name does not erase a known one.
        if !id.is_empty() {
            entry.id = id;
        }
        if !name.is_empty() {
            entry.name = name;
        }
    }

    pub fn on_delta(&mut self, index: usize, delta: &str) {
        // Some providers send no start, so a delta opens the entry itself.
        let entry = self.inflight.entry(index).or_default();
        entry.args.push_str(delta);
    }

    /// Returns the call at `index` with its parsed arguments and removes it, for a call whose
    /// arguments are whole (its block closed, or it came in one piece) and a stream whose stop
    /// reason is already known (`stop`) or never matters. `thought_signature` is the caller's:
    /// only the adapter reads it from the stream.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Parse`] when no call is at `index` or its id or name is empty, which is
    /// the provider's stream gone wrong; when the arguments are not a JSON object, the error
    /// [`UnparsedToolCall::into_error`] gives for `stop`.
    pub fn finalize(
        &mut self,
        index: usize,
        stop: Option<StopReason>,
        thought_signature: Option<String>,
    ) -> Result<ToolCall, ProviderError> {
        match self.end(index, ArgumentText::Whole, thought_signature)? {
            EndedCall::Parsed(call) => Ok(call),
            EndedCall::Unparsed(call) => Err(call.into_error(stop)),
        }
    }

    /// Ends the call at `index` like [`Self::finalize`], for a stream whose stop reason comes
    /// after the call's block closes (Anthropic). Arguments that are not a JSON object are judged
    /// at once when the stream already said why the turn stopped, and otherwise held for
    /// [`Self::take_held`] and `None` is returned.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Parse`] when no call is at `index` or its id or name is empty; the error
    /// [`UnparsedToolCall::into_error`] gives when the arguments are not an object and the stop
    /// reason is known.
    pub fn finalize_or_hold(
        &mut self,
        index: usize,
        thought_signature: Option<String>,
    ) -> Result<Option<ToolCall>, ProviderError> {
        match (
            self.end(index, ArgumentText::Whole, thought_signature)?,
            self.stop_reason_said(),
        ) {
            (EndedCall::Parsed(call), _) => Ok(Some(call)),
            (EndedCall::Unparsed(unparsed), Some(stop)) => Err(unparsed.into_error(Some(stop))),
            (EndedCall::Unparsed(unparsed), None) => {
                self.held.get_or_insert(unparsed);
                Ok(None)
            }
        }
    }

    /// Returns the held call whose arguments did not parse, for the caller to end with the
    /// turn's stop reason when it comes.
    pub fn take_held(&mut self) -> Option<UnparsedToolCall> {
        self.held.take()
    }

    /// Ends every call in flight, lowest index first, as a `ToolCallEnd` each, at the stop
    /// reason `stop`: a call is whole unless the output limit cut it ([`ArgumentText::at_stop`]),
    /// so empty arguments cut at `MaxTokens` are a truncation, not a call with no arguments.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Parse`] for a call with an empty id or name; the error
    /// [`UnparsedToolCall::into_error`] gives for the first call whose arguments are not an
    /// object.
    pub fn end_inflight(&mut self, stop: StopReason) -> Result<Vec<ChatEvent>, ProviderError> {
        self.end_each_inflight(ArgumentText::at_stop(stop), Some(stop))
    }

    /// Ends every call the stream left open where it ended (`end`): the held call first, then
    /// those in flight, each judged by the stop reason the stream said, or by none when it said
    /// none.
    ///
    /// # Errors
    ///
    /// [`ProviderError::StreamCut`], carrying the first call left open, when the body ended
    /// before the stream's end ([`stream_was_cut`]); the held call's error; else
    /// [`ProviderError::Parse`] for a call in flight with an empty id or name, or the error
    /// [`UnparsedToolCall::into_error`] gives for the first call in flight whose arguments are
    /// not an object, carrying its raw text.
    pub fn end_open_calls(&mut self, end: StreamEnd) -> Result<Vec<ChatEvent>, ProviderError> {
        if end == StreamEnd::Terminator {
            self.terminator_seen = true;
        }
        if stream_was_cut(end, self.terminator_seen, self.finished_emitted) {
            return Err(ProviderError::StreamCut {
                open_call: self.first_open_call(),
            });
        }
        let stop = self.stop_reason_said();
        if let Some(call) = self.held.take() {
            return Err(call.into_error(stop));
        }
        self.end_each_inflight(
            stop.map_or(ArgumentText::Whole, ArgumentText::at_stop),
            stop,
        )
    }

    /// Returns the call a cut left open: the held one, else the one in flight at the lowest index.
    fn first_open_call(&mut self) -> Option<ReceivedToolCall> {
        if let Some(held) = self.held.take() {
            return Some(ReceivedToolCall {
                id: held.id,
                name: held.name,
                raw: held.raw,
            });
        }
        let (_, call) = self.inflight.pop_first()?;
        Some(ReceivedToolCall {
            id: call.id,
            name: call.name,
            raw: call.args,
        })
    }

    fn end_each_inflight(
        &mut self,
        text: ArgumentText,
        stop: Option<StopReason>,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        self.inflight_indexes()
            .into_iter()
            .map(|index| match self.end(index, text, None)? {
                EndedCall::Parsed(call) => Ok(ChatEvent::ToolCallEnd {
                    index,
                    id: call.id,
                    parsed_args: call.arguments,
                    thought_signature: call.thought_signature,
                }),
                EndedCall::Unparsed(call) => Err(call.into_error(stop)),
            })
            .collect()
    }

    /// Returns the stop reason the stream has said so far, whether or not `Finished` went out
    /// with it.
    fn stop_reason_said(&self) -> Option<StopReason> {
        self.last_stop_reason.or(self.pending_stop_reason)
    }

    fn end(
        &mut self,
        index: usize,
        text: ArgumentText,
        thought_signature: Option<String>,
    ) -> Result<EndedCall, ProviderError> {
        let Entry::Occupied(entry) = self.inflight.entry(index) else {
            return Err(stream_error(ParseProblem::UnstartedToolCall { index }));
        };
        let acc = entry.get();

        // Without an id and a name the result cannot be routed back, and most providers refuse
        // the follow-up tool-result message.
        if acc.id.is_empty() || acc.name.is_empty() {
            return Err(stream_error(ParseProblem::UnidentifiedToolCall {
                index,
                id: acc.id.clone(),
                name: acc.name.clone(),
            }));
        }
        let acc = entry.remove();
        self.ended_calls += 1;
        Ok(match parse_arguments(&acc.args, text) {
            Ok(arguments) => EndedCall::Parsed(ToolCall {
                id: acc.id,
                name: acc.name,
                arguments,
                thought_signature,
            }),
            Err(problem) => EndedCall::Unparsed(UnparsedToolCall {
                id: acc.id,
                name: acc.name,
                raw: acc.args,
                problem,
            }),
        })
    }
}

/// Where a stream ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamEnd {
    /// At the event the protocol ends a stream with (Anthropic `message_stop`, OpenAI `[DONE]`).
    Terminator,
    /// Where the body ended, with no terminator.
    BodyEnd,
}

/// Returns whether the body ended before the stream's end: no terminator came, and no
/// `Finished` went out, which is where a protocol without a terminator (Gemini) ends.
fn stream_was_cut(end: StreamEnd, terminator_seen: bool, finished_emitted: bool) -> bool {
    end == StreamEnd::BodyEnd && !terminator_seen && !finished_emitted
}

/// How a call with an id and a name ended: its arguments parsed, or they did not and the stop
/// reason decides which error that is.
enum EndedCall {
    Parsed(ToolCall),
    Unparsed(UnparsedToolCall),
}

/// A tool call whose arguments did not parse into an object, before it is known why the model's
/// turn stopped: on Anthropic the call's block closes before the stop reason comes.
#[derive(Debug)]
pub struct UnparsedToolCall {
    pub id: String,
    pub name: String,
    pub raw: String,
    pub problem: MalformedArguments,
}

impl UnparsedToolCall {
    /// Returns the error the call ends in, given why the turn stopped (`None` when the stream
    /// ended without saying). Arguments that are whole JSON but not an object are the model's at
    /// any stop, since a cut cannot produce a whole value. Arguments that are not JSON, empty ones
    /// included, were cut by the output limit at `MaxTokens`, written wrong by the model at any
    /// other stop, and of unknown cause when no stop reason came.
    pub fn into_error(self, stop: Option<StopReason>) -> ProviderError {
        let Self {
            id,
            name,
            raw,
            problem,
        } = self;
        match (&problem, stop) {
            (MalformedArguments::NotJson(_), Some(StopReason::MaxTokens)) => {
                ProviderError::TruncatedToolCall { id, name, raw }
            }
            (MalformedArguments::NotJson(_), None) => {
                ProviderError::IncompleteToolCall { id, name, raw }
            }
            (MalformedArguments::NotAnObject(_), _) | (MalformedArguments::NotJson(_), Some(_)) => {
                ProviderError::MalformedToolCall {
                    id,
                    name,
                    raw,
                    problem,
                }
            }
        }
    }
}

/// Returns the error of a stream whose tool-call events do not fit together.
fn stream_error(problem: ParseProblem) -> ProviderError {
    ProviderError::Parse {
        at: "tool-call stream",
        line: None,
        problem: Box::new(problem),
        raw: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tars_types::JsonKind;

    fn half_written_call() -> UnparsedToolCall {
        let raw = r#"{"path":"a.txt","content":"lorem ip"#;
        UnparsedToolCall {
            id: "call_1".into(),
            name: "write_file".into(),
            raw: raw.into(),
            problem: MalformedArguments::NotJson(serde_json::from_str::<Value>(raw).unwrap_err()),
        }
    }

    #[test]
    fn arguments_cut_off_at_the_output_limit_are_a_truncation() {
        assert!(matches!(
            half_written_call().into_error(Some(StopReason::MaxTokens)),
            ProviderError::TruncatedToolCall { ref raw, .. } if raw.ends_with("lorem ip")
        ));
    }

    #[test]
    fn unparsable_arguments_at_any_other_stop_are_the_models() {
        for stop in [StopReason::ToolUse, StopReason::EndTurn, StopReason::Other] {
            assert!(matches!(
                half_written_call().into_error(Some(stop)),
                ProviderError::MalformedToolCall { .. }
            ));
        }
    }

    #[test]
    fn unparsable_arguments_with_no_stop_reason_are_incomplete_with_the_raw_text() {
        assert!(matches!(
            half_written_call().into_error(None),
            ProviderError::IncompleteToolCall { ref raw, .. } if raw.ends_with("lorem ip")
        ));
    }

    #[test]
    fn whole_json_that_is_not_an_object_is_the_models_at_any_stop() {
        for stop in [None, Some(StopReason::MaxTokens), Some(StopReason::ToolUse)] {
            let call = UnparsedToolCall {
                id: "call_1".into(),
                name: "f".into(),
                raw: "[1,2]".into(),
                problem: MalformedArguments::NotAnObject(JsonKind::Array),
            };
            assert!(
                matches!(
                    call.into_error(stop),
                    ProviderError::MalformedToolCall { .. }
                ),
                "{stop:?}"
            );
        }
    }

    #[test]
    fn empty_prefix_is_a_truncation_at_the_output_limit_and_incomplete_with_no_stop_reason() {
        let call = |stop| {
            let problem = parse_arguments("", ArgumentText::Prefix).unwrap_err();
            UnparsedToolCall {
                id: "call_1".into(),
                name: "write_file".into(),
                raw: String::new(),
                problem,
            }
            .into_error(stop)
        };
        assert!(matches!(
            call(None),
            ProviderError::IncompleteToolCall { ref raw, .. } if raw.is_empty()
        ));
        assert!(matches!(
            call(Some(StopReason::MaxTokens)),
            ProviderError::TruncatedToolCall { ref raw, .. } if raw.is_empty()
        ));
    }

    #[test]
    fn accumulates_split_args_then_parses() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "search".into());
        b.on_delta(0, "{\"q\":\"r");
        b.on_delta(0, "ust\"}");
        let call = b.finalize(0, None, None).unwrap();
        assert_eq!(call.id, "c1");
        assert_eq!(call.name, "search");
        assert_eq!(Value::Object(call.arguments), json!({"q": "rust"}));
    }

    #[test]
    fn empty_args_become_empty_object() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "ping".into());
        let call = b.finalize(0, None, None).unwrap();
        assert!(call.arguments.is_empty());
    }

    #[test]
    fn parallel_calls_keyed_by_index() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "a".into(), "f1".into());
        b.on_start(1, "b".into(), "f2".into());
        b.on_delta(0, "{\"x\":1}");
        b.on_delta(1, "{\"y\":2}");
        let first = b.finalize(0, None, None).unwrap();
        let second = b.finalize(1, None, None).unwrap();
        assert_eq!(first.name, "f1");
        assert_eq!(Value::Object(first.arguments), json!({"x":1}));
        assert_eq!(second.name, "f2");
        assert_eq!(Value::Object(second.arguments), json!({"y":2}));
    }

    #[test]
    fn arguments_that_are_not_an_object_are_the_models_malformed_call_carrying_the_raw_text() {
        for raw in [r#""{\"q\":\"rust\"}""#, "[1,2]", "7", "null"] {
            let mut b = StreamDecodeState::new();
            b.on_start(0, "c1".into(), "f".into());
            b.on_delta(0, raw);
            match b.finalize(0, None, None) {
                Err(ProviderError::MalformedToolCall {
                    id,
                    name,
                    raw: got,
                    problem: MalformedArguments::NotAnObject(_),
                }) => assert_eq!((id.as_str(), name.as_str(), got.as_str()), ("c1", "f", raw)),
                other => panic!("{raw}: expected a malformed tool call, got {other:?}"),
            }
        }
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "f".into());
        b.on_delta(0, r#"{"q":"rust"}"#);
        assert!(b.finalize(0, None, None).is_ok());
    }

    #[test]
    fn arguments_cut_off_by_the_output_limit_are_a_truncation_not_the_models_error() {
        let half = r#"{"path":"a.txt","content":"lorem ip"#;
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "write_file".into());
        b.on_delta(0, half);
        assert!(matches!(
            b.finalize(0, Some(StopReason::MaxTokens), None),
            Err(ProviderError::TruncatedToolCall { ref raw, .. }) if raw == half
        ));

        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "write_file".into());
        b.on_delta(0, half);
        assert!(matches!(
            b.finalize(0, Some(StopReason::ToolUse), None),
            Err(ProviderError::MalformedToolCall { .. })
        ));
    }

    #[test]
    fn a_call_closed_before_the_stop_reason_is_held_and_a_parsed_one_is_not() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c0".into(), "f".into());
        b.on_delta(0, r#"{"q":1}"#);
        b.on_start(1, "c1".into(), "g".into());
        b.on_delta(1, r#"{"q":"#);
        assert!(b.finalize_or_hold(0, None).unwrap().is_some());
        assert!(b.finalize_or_hold(1, None).unwrap().is_none());
        let held = b.take_held().unwrap();
        assert_eq!((held.id.as_str(), held.raw.as_str()), ("c1", r#"{"q":"#));
        assert!(b.take_held().is_none());
    }

    #[test]
    fn finalize_unknown_index_errors() {
        let mut b = StreamDecodeState::new();
        let err = b.finalize(99, None, None).unwrap_err();
        assert!(matches!(err, ProviderError::Parse { .. }));
    }

    #[test]
    fn arguments_that_are_not_json_at_a_tool_use_stop_are_the_models_malformed_call() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "f".into());
        b.on_delta(0, "not json at all");
        let err = b.finalize(0, Some(StopReason::ToolUse), None).unwrap_err();
        assert!(matches!(
            err,
            ProviderError::MalformedToolCall {
                problem: MalformedArguments::NotJson(_),
                ..
            }
        ));
    }

    #[test]
    fn a_call_without_a_name_is_the_providers_stream_gone_wrong() {
        let mut b = StreamDecodeState::new();
        b.on_delta(0, "{}");
        assert!(matches!(
            b.finalize(0, None, None),
            Err(ProviderError::Parse { .. })
        ));
    }

    #[test]
    fn inflight_indexes_lists_every_call_lowest_first() {
        let mut b = StreamDecodeState::new();
        b.on_delta(40, "{}");
        b.on_start(0, "a".into(), "f".into());
        b.on_start(31, "b".into(), "f".into());
        b.on_start(32, "c".into(), "f".into());
        assert_eq!(b.inflight_indexes(), vec![0, 31, 32, 40]);
        b.finalize(31, None, None).unwrap();
        assert_eq!(b.inflight_indexes(), vec![0, 32, 40]);
    }

    #[test]
    fn end_inflight_ends_a_call_at_any_index() {
        let mut b = StreamDecodeState::new();
        b.on_start(31, "a".into(), "f".into());
        b.on_start(32, "b".into(), "g".into());
        let ended: Vec<usize> = b
            .end_inflight(StopReason::ToolUse)
            .unwrap()
            .iter()
            .map(|e| match e {
                ChatEvent::ToolCallEnd { index, .. } => *index,
                other => panic!("expected ToolCallEnd, got {other:?}"),
            })
            .collect();
        assert_eq!(ended, vec![31, 32]);
        assert!(b.inflight_indexes().is_empty());
    }

    const HALF: &str = r#"{"path":"a.txt","content":"lorem ip"#;

    #[test]
    fn a_call_still_in_flight_when_the_body_ends_is_a_cut_stream_with_its_raw_text() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "write_file".into());
        b.on_delta(0, HALF);
        match b.end_open_calls(StreamEnd::BodyEnd) {
            Err(ProviderError::StreamCut {
                open_call: Some(ReceivedToolCall { id, name, raw }),
            }) => {
                assert_eq!(
                    (id.as_str(), name.as_str(), raw.as_str()),
                    ("c1", "write_file", HALF)
                );
            }
            other => panic!("expected a cut stream, got {other:?}"),
        }
    }

    #[test]
    fn a_call_whose_arguments_parse_is_still_a_cut_stream_when_the_body_ends() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "f".into());
        b.on_delta(0, r#"{"q":1}"#);
        assert!(matches!(
            b.end_open_calls(StreamEnd::BodyEnd),
            Err(ProviderError::StreamCut { open_call: Some(_) })
        ));
    }

    #[test]
    fn a_body_that_ends_with_no_call_open_before_the_streams_end_is_a_cut_stream() {
        let mut b = StreamDecodeState::new();
        assert!(matches!(
            b.end_open_calls(StreamEnd::BodyEnd),
            Err(ProviderError::StreamCut { open_call: None })
        ));
    }

    #[test]
    fn a_held_call_is_the_open_call_of_a_cut_stream() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c0".into(), "f".into());
        b.on_delta(0, HALF);
        b.on_start(1, "c1".into(), "g".into());
        assert!(b.finalize_or_hold(0, None).unwrap().is_none());
        assert!(matches!(
            b.end_open_calls(StreamEnd::BodyEnd),
            Err(ProviderError::StreamCut { open_call: Some(ReceivedToolCall { ref id, .. }) }) if id == "c0"
        ));
    }

    #[test]
    fn a_body_that_ends_after_the_terminator_or_a_finished_is_not_cut() {
        let mut b = StreamDecodeState::new();
        assert!(b.end_open_calls(StreamEnd::Terminator).unwrap().is_empty());
        assert!(b.end_open_calls(StreamEnd::BodyEnd).unwrap().is_empty());

        let mut b = StreamDecodeState::new();
        b.mark_finished();
        assert!(b.end_open_calls(StreamEnd::BodyEnd).unwrap().is_empty());
    }

    #[test]
    fn only_a_body_end_before_a_terminator_and_a_finished_is_a_cut() {
        assert!(stream_was_cut(StreamEnd::BodyEnd, false, false));
        assert!(!stream_was_cut(StreamEnd::BodyEnd, true, false));
        assert!(!stream_was_cut(StreamEnd::BodyEnd, false, true));
        assert!(!stream_was_cut(StreamEnd::Terminator, false, false));
    }

    #[test]
    fn a_call_still_in_flight_whose_arguments_parse_ends_as_received_at_the_terminator() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "f".into());
        b.on_delta(0, r#"{"q":1}"#);
        match b.end_open_calls(StreamEnd::Terminator).unwrap().as_slice() {
            [
                ChatEvent::ToolCallEnd {
                    id, parsed_args, ..
                },
            ] => {
                assert_eq!(id, "c1");
                assert_eq!(Value::Object(parsed_args.clone()), json!({"q": 1}));
            }
            other => panic!("expected one ToolCallEnd, got {other:?}"),
        }
    }

    #[test]
    fn a_call_in_flight_at_the_end_is_judged_by_the_stop_reason_the_stream_said() {
        for (said, truncated) in [(StopReason::MaxTokens, true), (StopReason::ToolUse, false)] {
            let mut b = StreamDecodeState::new();
            b.on_start(0, "c1".into(), "write_file".into());
            b.on_delta(0, HALF);
            b.record_finish_state(Some(said), None);
            let err = b.end_open_calls(StreamEnd::Terminator).unwrap_err();
            assert_eq!(
                matches!(err, ProviderError::TruncatedToolCall { .. }),
                truncated,
                "{said:?}: {err:?}"
            );
            assert_eq!(
                matches!(err, ProviderError::MalformedToolCall { .. }),
                !truncated,
                "{said:?}: {err:?}"
            );
        }
    }

    #[test]
    fn a_held_call_is_judged_by_the_stop_reason_the_stream_said_at_the_end() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "write_file".into());
        b.on_delta(0, HALF);
        assert!(b.finalize_or_hold(0, None).unwrap().is_none());
        assert!(matches!(
            b.end_open_calls(StreamEnd::Terminator),
            Err(ProviderError::IncompleteToolCall { .. })
        ));

        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "write_file".into());
        b.on_delta(0, HALF);
        assert!(b.finalize_or_hold(0, None).unwrap().is_none());
        b.record_pending_stop(StopReason::MaxTokens);
        assert!(matches!(
            b.end_open_calls(StreamEnd::Terminator),
            Err(ProviderError::TruncatedToolCall { .. })
        ));
    }

    #[test]
    fn a_block_that_closes_after_the_stop_reason_is_judged_at_once_and_not_held() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "write_file".into());
        b.on_delta(0, HALF);
        b.record_finish_state(Some(StopReason::MaxTokens), None);
        assert!(matches!(
            b.finalize_or_hold(0, None),
            Err(ProviderError::TruncatedToolCall { ref raw, .. }) if raw == HALF
        ));
        assert!(b.take_held().is_none());
    }

    #[test]
    fn a_call_in_flight_with_no_arguments_is_open_with_none_when_the_body_ends() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "write_file".into());
        match b.end_open_calls(StreamEnd::BodyEnd) {
            Err(ProviderError::StreamCut {
                open_call: Some(ReceivedToolCall { id, raw, .. }),
            }) => {
                assert_eq!((id.as_str(), raw.as_str()), ("c1", ""));
            }
            other => panic!("expected a cut stream, got {other:?}"),
        }
    }

    #[test]
    fn a_call_in_flight_with_no_arguments_is_a_call_without_arguments_once_the_output_is_over() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "ping".into());
        let at_terminator = b.end_open_calls(StreamEnd::Terminator).unwrap();
        assert!(matches!(
            at_terminator.as_slice(),
            [ChatEvent::ToolCallEnd { parsed_args, .. }] if parsed_args.is_empty()
        ));

        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "ping".into());
        b.record_finish_state(Some(StopReason::ToolUse), None);
        let after_stop = b.end_open_calls(StreamEnd::Terminator).unwrap();
        assert!(matches!(
            after_stop.as_slice(),
            [ChatEvent::ToolCallEnd { parsed_args, .. }] if parsed_args.is_empty()
        ));
    }

    #[test]
    fn a_call_in_flight_with_no_arguments_is_a_truncation_when_the_output_limit_ended_it() {
        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "write_file".into());
        assert!(matches!(
            b.end_inflight(StopReason::MaxTokens),
            Err(ProviderError::TruncatedToolCall { ref raw, .. }) if raw.is_empty()
        ));

        let mut b = StreamDecodeState::new();
        b.on_start(0, "c1".into(), "write_file".into());
        b.record_finish_state(Some(StopReason::MaxTokens), None);
        assert!(matches!(
            b.end_open_calls(StreamEnd::Terminator),
            Err(ProviderError::TruncatedToolCall { ref raw, .. }) if raw.is_empty()
        ));
    }

    #[test]
    fn the_recorded_end_is_finished_with_a_stop_reason_and_the_usage_without_one() {
        let mut b = StreamDecodeState::new();
        assert!(b.recorded_end().is_none());
        b.record_finish_state(
            None,
            Some(Usage {
                output_tokens: 5,
                ..Usage::default()
            }),
        );
        assert!(matches!(
            b.recorded_end(),
            Some(ChatEvent::UsageProgress { usage }) if usage.output_tokens == 5
        ));
        b.record_finish_state(Some(StopReason::EndTurn), None);
        assert!(matches!(
            b.recorded_end(),
            Some(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                usage,
            }) if usage.unwrap().output_tokens == 5
        ));
    }

    #[test]
    fn a_stream_that_ends_with_nothing_open_ends_with_nothing() {
        let mut b = StreamDecodeState::new();
        b.record_finish_state(Some(StopReason::EndTurn), None);
        assert!(b.end_open_calls(StreamEnd::Terminator).unwrap().is_empty());
    }
}
