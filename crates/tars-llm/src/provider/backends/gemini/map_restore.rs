//! Turns the arrays of `{key, value}` entries Gemini was sent in place of maps back into maps, in
//! the structured output and in tool-call arguments, so a caller reads the shape its own schema
//! describes. It does not judge the reply: a part that is not an entry array is passed on as the
//! model wrote it.

use std::collections::HashMap;

use futures::StreamExt;
use serde_json::Value;
use tars_types::{ChatEvent, ChatRequest};

use crate::ProviderError;
use crate::provider::LlmEventStream;
use crate::provider::schema_adapt::{MapPath, restore_field_maps, restore_maps};

use super::adapter::{response_schema, tool_parameters};

/// Where the request's schemas sent a map as an array: in the structured output, and in each
/// tool's arguments by tool name.
#[derive(Debug, Default)]
pub(super) struct EncodedMaps {
    response: Vec<MapPath>,
    tools: HashMap<String, Vec<MapPath>>,
}

impl EncodedMaps {
    /// Returns where `req`'s schemas, adapted as the request body adapts them, encode maps.
    ///
    /// # Errors
    ///
    /// The `InvalidRequest` the request body's translation would refuse the same schema with.
    pub(super) fn of(req: &ChatRequest) -> Result<Self, ProviderError> {
        let response = response_schema(req)?
            .map(|s| s.encoded_maps)
            .unwrap_or_default();
        let tools = req
            .tools
            .iter()
            .map(|t| Ok((t.name.clone(), tool_parameters(t)?.encoded_maps)))
            .filter(|r| !matches!(r, Ok((_, maps)) if maps.is_empty()))
            .collect::<Result<_, ProviderError>>()?;
        Ok(Self { response, tools })
    }

    fn is_empty(&self) -> bool {
        self.response.is_empty() && self.tools.is_empty()
    }
}

/// Returns `json_text` with the maps at `maps` restored, or unchanged when it is not JSON.
fn restored_text(json_text: &str, maps: &[MapPath]) -> String {
    match serde_json::from_str::<Value>(json_text) {
        Ok(mut v) => {
            restore_maps(&mut v, maps);
            v.to_string()
        }
        Err(_) => json_text.to_string(),
    }
}

/// Returns `inner` with its maps restored. With maps in the structured output, the reply's text
/// is held until the stream ends or fails, since only the whole text parses; it is then sent as
/// one `Delta` before the event that ended it.
pub(super) fn restore_in_stream(inner: LlmEventStream, maps: EncodedMaps) -> LlmEventStream {
    if maps.is_empty() {
        return inner;
    }
    Box::pin(async_stream::stream! {
        let mut inner = inner;
        let mut tool_of_index: HashMap<usize, String> = HashMap::new();
        let mut held: Option<String> = (!maps.response.is_empty()).then(String::new);
        let tool_maps = |names: &HashMap<usize, String>, index: usize| -> &[MapPath] {
            names
                .get(&index)
                .and_then(|name| maps.tools.get(name))
                .map_or(&[], Vec::as_slice)
        };
        while let Some(item) = inner.next().await {
            match item {
                Ok(ChatEvent::Delta { text }) if held.is_some() => {
                    if let Some(h) = held.as_mut() {
                        h.push_str(&text);
                    }
                }
                Ok(ChatEvent::ToolCallStart { index, id, name }) => {
                    tool_of_index.insert(index, name.clone());
                    yield Ok(ChatEvent::ToolCallStart { index, id, name });
                }
                Ok(ChatEvent::ToolCallArgsDelta { index, args_delta }) => {
                    let paths = tool_maps(&tool_of_index, index);
                    let args_delta = if paths.is_empty() {
                        args_delta
                    } else {
                        restored_text(&args_delta, paths)
                    };
                    yield Ok(ChatEvent::ToolCallArgsDelta { index, args_delta });
                }
                Ok(ChatEvent::ToolCallEnd { index, id, mut parsed_args, thought_signature }) => {
                    restore_field_maps(&mut parsed_args, tool_maps(&tool_of_index, index));
                    yield Ok(ChatEvent::ToolCallEnd { index, id, parsed_args, thought_signature });
                }
                Ok(ev @ ChatEvent::Finished { .. }) => {
                    if let Some(text) = held.take().filter(|t| !t.is_empty()) {
                        yield Ok(ChatEvent::Delta { text: restored_text(&text, &maps.response) });
                    }
                    yield Ok(ev);
                }
                Ok(ev) => yield Ok(ev),
                Err(e) => {
                    if let Some(text) = held.take().filter(|t| !t.is_empty()) {
                        yield Ok(ChatEvent::Delta { text: restored_text(&text, &maps.response) });
                    }
                    yield Err(e);
                }
            }
        }
        if let Some(text) = held.take().filter(|t| !t.is_empty()) {
            yield Ok(ChatEvent::Delta { text: restored_text(&text, &maps.response) });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::http_base::{HttpAdapter, HttpOverrides, SseEvent};
    use crate::provider::tool_buffer::StreamDecodeState;
    use serde_json::json;
    use tars_types::{JsonSchema, ToolSpec};

    use super::super::adapter::GeminiAdapter;

    fn scores_schema() -> serde_json::Map<String, Value> {
        tars_types::json_object! {
            "type": "object",
            "properties": {
                "scores": {"type": "object", "additionalProperties": {"type": "integer"}}
            },
            "required": ["scores"]
        }
    }

    /// Decodes Gemini's SSE chunks the way a stream does, then restores the request's maps.
    async fn decode(req: &ChatRequest, chunks: &[Value]) -> Vec<ChatEvent> {
        let adapter = GeminiAdapter::new("https://x".into(), HttpOverrides::default());
        let mut buf = StreamDecodeState::new();
        let mut events = Vec::new();
        for chunk in chunks {
            let raw = SseEvent {
                event: "message".into(),
                data: chunk.to_string(),
            };
            events.extend(adapter.parse_event(&raw, &mut buf).unwrap());
        }
        let inner: LlmEventStream = Box::pin(futures::stream::iter(events.into_iter().map(Ok)));
        restore_in_stream(inner, EncodedMaps::of(req).unwrap())
            .map(Result::unwrap)
            .collect()
            .await
    }

    #[tokio::test]
    async fn a_structured_output_map_comes_back_as_the_map_the_callers_schema_describes() {
        let mut req = ChatRequest::user("score them");
        req.structured_output = Some(JsonSchema::strict("Scores", scores_schema()));
        // The reply in the shape the adapted schema asked Gemini for, split over two chunks.
        let chunks = [
            json!({"candidates": [{"content": {"parts": [
                {"text": "{\"scores\": [{\"key\": \"a\", \"value\": 1},"}
            ]}}]}),
            json!({"candidates": [{"content": {"parts": [
                {"text": " {\"key\": \"b\", \"value\": 2}]}"}
            ]}, "finishReason": "STOP"}],
             "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4}}),
        ];
        let events = decode(&req, &chunks).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply, json!({"scores": {"a": 1, "b": 2}}));
        assert!(
            matches!(events.last(), Some(ChatEvent::Finished { .. })),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_tool_call_argument_map_comes_back_as_the_map_the_tools_schema_describes() {
        let mut req = ChatRequest::user("score them");
        req.tools = vec![
            ToolSpec::new(
                "score",
                "scores",
                JsonSchema::strict("Args", scores_schema()),
            )
            .unwrap(),
        ];
        let chunks = [json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "score", "args": {"scores": [{"key": "a", "value": 1}]}}}
        ]}, "finishReason": "STOP"}]})];
        let events = decode(&req, &chunks).await;
        let end = events
            .iter()
            .find_map(|e| match e {
                ChatEvent::ToolCallEnd { parsed_args, .. } => {
                    Some(Value::Object(parsed_args.clone()))
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(end, json!({"scores": {"a": 1}}));
        let delta = events
            .iter()
            .find_map(|e| match e {
                ChatEvent::ToolCallArgsDelta { args_delta, .. } => Some(args_delta.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&delta).unwrap(),
            json!({"scores": {"a": 1}})
        );
    }

    #[tokio::test]
    async fn a_request_without_maps_passes_the_stream_through_untouched() {
        let req = ChatRequest::user("hi");
        let chunks = [
            json!({"candidates": [{"content": {"parts": [{"text": "a"}]}}]}),
            json!({"candidates": [{"content": {"parts": [{"text": "b"}]}}]}),
        ];
        let events = decode(&req, &chunks).await;
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["a", "b"], "{events:?}");
    }
}
