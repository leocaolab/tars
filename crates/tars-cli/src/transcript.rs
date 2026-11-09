//! Folds the events one streamed call sends into what `probe` and `bench` judge it by, so the
//! two read "the stream ended without `Finished`" the same way.

use std::time::Duration;

use tars_types::{ChatEvent, StopReason, Usage};

/// What a call's stream has sent so far.
#[derive(Clone, Debug, Default)]
pub(crate) struct Transcript {
    pub(crate) events: usize,
    pub(crate) text: String,
    pub(crate) thinking: String,
    pub(crate) tool_calls: usize,
    /// When the first generated token, visible or reasoning, arrived after the request.
    pub(crate) first_token: Option<Duration>,
    /// The `Finished` event's stop reason and usage; `None` while the stream has not finished.
    pub(crate) finished: Option<(StopReason, Usage)>,
}

impl Transcript {
    /// Returns the transcript after `event`, which arrived `at` after the request was sent.
    pub(crate) fn step(mut self, event: ChatEvent, at: Duration) -> Self {
        self.events += 1;
        match event {
            ChatEvent::Delta { text } => {
                self.first_token.get_or_insert(at);
                self.text.push_str(&text);
            }
            ChatEvent::ThinkingDelta { text } => {
                self.first_token.get_or_insert(at);
                self.thinking.push_str(&text);
            }
            ChatEvent::ToolCallStart { .. } => self.tool_calls += 1,
            ChatEvent::Finished { stop_reason, usage } => {
                self.finished = Some((stop_reason, usage));
            }
            ChatEvent::Degraded { .. }
            | ChatEvent::Started { .. }
            | ChatEvent::ToolCallArgsDelta { .. }
            | ChatEvent::ToolCallEnd { .. }
            | ChatEvent::UsageProgress { .. }
            | ChatEvent::Measured { .. }
            | ChatEvent::Validated { .. } => {}
        }
        self
    }
}

/// How a stream that ended without an error went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamEnd {
    /// It finished with text or a tool call.
    Answered,
    /// It ended without a `Finished` event.
    Unfinished,
    /// It finished with neither text nor a tool call.
    Empty,
}

/// Says how a stream went from whether it finished, sent text and called a tool. A reply made
/// only of tool calls has no text and is an answer.
pub(crate) fn stream_end(finished: bool, has_text: bool, has_tool_call: bool) -> StreamEnd {
    match (finished, has_text || has_tool_call) {
        (false, _) => StreamEnd::Unfinished,
        (true, false) => StreamEnd::Empty,
        (true, true) => StreamEnd::Answered,
    }
}

/// Returns the line `probe` prints for event number `n`.
pub(crate) fn event_line(n: usize, event: &ChatEvent) -> String {
    let body = match event {
        ChatEvent::Degraded { unsent } => format!("Degraded    unsent={unsent:?}"),
        ChatEvent::Started { actual_model, .. } => format!("Started     model={actual_model}"),
        ChatEvent::Delta { text } => format!("Delta       text={text:?}"),
        ChatEvent::ThinkingDelta { text } => format!("Thinking    text={text:?}"),
        ChatEvent::ToolCallStart { id, name, .. } => format!("ToolStart   id={id} name={name}"),
        ChatEvent::ToolCallArgsDelta { index, args_delta } => {
            format!("ToolArgs    idx={index} delta={args_delta:?}")
        }
        ChatEvent::ToolCallEnd { id, .. } => format!("ToolEnd     id={id}"),
        ChatEvent::Measured { telemetry } => format!("Measured    {telemetry:?}"),
        ChatEvent::Validated { summary } => format!("Validated   {summary:?}"),
        ChatEvent::UsageProgress { usage } => format!("UsageProg   {usage:?}"),
        ChatEvent::Finished { stop_reason, usage } => format!(
            "Finished    stop={stop_reason:?} in={} out={} cached={} thinking={}",
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.thinking_tokens,
        ),
    };
    format!("[evt {n:>2}] {body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_is_answered_only_when_it_finished_with_text_or_a_tool_call() {
        assert_eq!(stream_end(false, true, true), StreamEnd::Unfinished);
        assert_eq!(stream_end(true, false, false), StreamEnd::Empty);
        assert_eq!(stream_end(true, true, false), StreamEnd::Answered);
        assert_eq!(stream_end(true, false, true), StreamEnd::Answered);
    }

    #[test]
    fn the_first_token_is_the_first_delta_visible_or_reasoning() {
        let ms = Duration::from_millis;
        let t = Transcript::default()
            .step(ChatEvent::ThinkingDelta { text: "hm".into() }, ms(40))
            .step(ChatEvent::Delta { text: "hi".into() }, ms(90))
            .step(
                ChatEvent::Finished {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                },
                ms(120),
            );
        assert_eq!(t.first_token, Some(ms(40)));
        assert_eq!(
            (t.events, t.text.as_str(), t.thinking.as_str()),
            (3, "hi", "hm")
        );
        assert!(t.finished.is_some());
        assert_eq!(Transcript::default().first_token, None);
    }
}
