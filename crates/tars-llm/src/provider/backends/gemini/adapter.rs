//! Translates requests into Gemini's `streamGenerateContent` body, its stream events into
//! [`ChatEvent`]s, and its HTTP errors into [`ProviderError`]s. Gemini's tool result names the
//! function, which tars's tool message does not carry, so a call's id is `<name>@<id>` and a tool
//! result without that shape is refused.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use url::Url;

use crate::ProviderError;
use crate::provider::error::{
    CredentialError, CredentialKind, Feature, ParseProblem, ProviderRefusal, RequestRefusal,
    status_error,
};
use tars_types::{ChatEvent, ChatRequest, ContentBlock, ImageData, Message, StopReason};

use crate::provider::auth::ResolvedAuth;
use crate::provider::http_base::{HttpAdapter, HttpOverrides, SseEvent, parse_url};
use crate::provider::schema_adapt::{AdaptedSchema, SchemaDialect, adapt_schema};
use crate::provider::tool_buffer::StreamDecodeState;

use super::mapping::{map_stop_reason, parse_usage, truncate};

const API_VERSION: &str = "v1beta";

/// Returns whether the model can turn thinking off, read from its `data/provider.toml` row; an
/// unknown model cannot, since omitting the thinking config is always accepted.
fn model_supports_thinking_off(model: &str) -> bool {
    tars_config::MODEL_KB
        .find(model)
        .map(|m| !m.is_thinking_only())
        .unwrap_or(false)
}

/// The adapter of one Gemini provider: it translates requests, parses events and puts the
/// request's API key in the `x-goog-api-key` header.
pub struct GeminiAdapter {
    pub(super) base_url: String,
    pub(super) extras: HttpOverrides,
}

const API_KEY_HEADER: &str = "x-goog-api-key";

/// Returns the `x-goog-api-key` value of a request's credential. The key never goes in the URL,
/// because a failed request's error text carries its URL to logs and reports. The value is
/// marked sensitive, so a `HeaderMap`'s `Debug` does not print it.
///
/// # Errors
///
/// `Auth` when the credential is not a non-empty API key that is a valid header value; this
/// backend does not take a bearer token (Vertex AI ADC).
fn api_key_header(auth: &ResolvedAuth) -> Result<HeaderValue, ProviderError> {
    let key = match auth {
        ResolvedAuth::ApiKey(k) if k.is_empty() => {
            return Err(ProviderError::Auth(CredentialError::EmptyKey));
        }
        ResolvedAuth::ApiKey(k) => k,
        ResolvedAuth::Bearer(_) => return Err(wrong_credential(CredentialKind::Bearer)),
        ResolvedAuth::None => return Err(wrong_credential(CredentialKind::None)),
    };
    let mut value = HeaderValue::from_str(key).map_err(|source| {
        ProviderError::Auth(CredentialError::Header {
            header: API_KEY_HEADER,
            source,
        })
    })?;
    value.set_sensitive(true);
    Ok(value)
}

fn wrong_credential(got: CredentialKind) -> ProviderError {
    ProviderError::Auth(CredentialError::Kind {
        expected: CredentialKind::ApiKey,
        got,
    })
}

impl GeminiAdapter {
    pub(super) fn new(base_url: String, extras: HttpOverrides) -> Self {
        Self { base_url, extras }
    }

    fn translate_part(b: &ContentBlock) -> Value {
        match b {
            ContentBlock::Text { text } => json!({"text": text}),
            ContentBlock::Image { mime, data } => match data {
                ImageData::Base64(b) => json!({
                    "inline_data": {
                        "mime_type": mime,
                        "data": b,
                    }
                }),
                ImageData::Url(u) => json!({
                    "file_data": {
                        "mime_type": mime,
                        "file_uri": u,
                    }
                }),
            },
        }
    }

    fn translate_message(m: &Message) -> Result<Value, ProviderError> {
        match m {
            Message::User { content } => {
                let parts: Vec<Value> = content.iter().map(Self::translate_part).collect();
                Ok(json!({"role": "user", "parts": parts}))
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut parts: Vec<Value> = content.iter().map(Self::translate_part).collect();
                for tc in tool_calls {
                    let mut part = json!({
                        "functionCall": {
                            "name": tc.name,
                            "args": tc.arguments,
                        }
                    });
                    // Thinking models reject a replayed call without its signature.
                    if let Some(sig) = &tc.thought_signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    parts.push(part);
                }
                Ok(json!({"role": "model", "parts": parts}))
            }
            Message::Tool {
                tool_call_id,
                content,
                is_error,
            } => {
                // Refused here because Gemini's own 400 only says "functionResponse.name is
                // required".
                let name = match tool_call_id.split_once('@') {
                    Some((n, _)) if !n.is_empty() => n,
                    _ => {
                        return Err(ProviderError::InvalidRequest(
                            RequestRefusal::ToolCallIdWithoutName {
                                tool_call_id: tool_call_id.clone(),
                            },
                        ));
                    }
                };
                let text = tool_result_text(content)?;
                // `functionResponse` has no error flag; the key tells the model it failed.
                let response = if *is_error {
                    json!({"error": text})
                } else {
                    json!({"output": text})
                };
                Ok(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": name,
                            "response": response,
                        }
                    }]
                }))
            }
            Message::System { content: _ } => {
                // Gemini has no system role; sent as a user message, the instruction would weigh
                // differently.
                Err(ProviderError::InvalidRequest(RequestRefusal::Unsupported(
                    Feature::SystemMessage,
                )))
            }
        }
    }

    fn translate_tools(tools: &[tars_types::ToolSpec]) -> Result<Value, ProviderError> {
        let declarations: Vec<Value> = tools
            .iter()
            .map(|t| {
                Ok(json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": tool_parameters(t)?.schema,
                }))
            })
            .collect::<Result<_, ProviderError>>()?;
        Ok(json!([{"functionDeclarations": declarations}]))
    }
}

/// Returns a tool's parameters in Gemini's dialect, which rejects standard keywords such as
/// `additionalProperties` and `$schema` there as in `responseSchema`.
pub(super) fn tool_parameters(t: &tars_types::ToolSpec) -> Result<AdaptedSchema, ProviderError> {
    adapt_schema(&t.input_schema.to_value(), SchemaDialect::Gemini).map_err(|source| {
        ProviderError::InvalidRequest(RequestRefusal::ToolSchema {
            tool: t.name.clone(),
            source,
        })
    })
}

/// Returns the request's structured-output schema in Gemini's dialect, when it asks for one.
pub(super) fn response_schema(req: &ChatRequest) -> Result<Option<AdaptedSchema>, ProviderError> {
    req.structured_output
        .as_ref()
        .map(|schema| {
            adapt_schema(&schema.to_value(), SchemaDialect::Gemini)
                .map_err(|e| ProviderError::InvalidRequest(RequestRefusal::ResponseSchema(e)))
        })
        .transpose()
}

#[async_trait]
impl HttpAdapter for GeminiAdapter {
    fn build_url(&self, model: &str) -> Result<Url, ProviderError> {
        let trimmed = self.base_url.trim_end_matches('/');
        parse_url(format!(
            "{trimmed}/{API_VERSION}/models/{model}:streamGenerateContent?alt=sse"
        ))
    }

    fn build_headers(&self, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(API_KEY_HEADER, api_key_header(auth)?);
        Ok(h)
    }

    fn translate_request(&self, req: &ChatRequest, model: &str) -> Result<Value, ProviderError> {
        if req.messages.is_empty() {
            return Err(ProviderError::InvalidRequest(RequestRefusal::EmptyMessages));
        }

        let contents: Vec<Value> = req
            .messages
            .iter()
            .map(Self::translate_message)
            .collect::<Result<_, _>>()?;

        let mut body = json!({
            "contents": contents,
        });

        let mut config = serde_json::Map::new();
        if let Some(max) = req.max_output_tokens {
            config.insert("maxOutputTokens".into(), json!(max));
        }
        if let Some(t) = req.temperature {
            config.insert("temperature".into(), json!(t));
        }
        if !req.stop_sequences.is_empty() {
            config.insert("stopSequences".into(), json!(req.stop_sequences));
        }
        if let Some(seed) = req.seed {
            config.insert("seed".into(), json!(seed));
        }

        if let Some(schema) = response_schema(req)? {
            config.insert("responseMimeType".into(), json!("application/json"));
            config.insert("responseSchema".into(), schema.schema);
        }

        // Gemini 2.5 takes a numeric `thinkingBudget`, 3.x a `thinkingLevel`; the model's row in
        // `data/provider.toml` says which, and a row that does not is a budget. A thinking-only
        // model (gemini-2.5-pro) refuses a budget of 0 with HTTP 400 "Budget 0 is invalid";
        // every model takes a positive or dynamic (-1) one.
        let param = tars_config::MODEL_KB
            .find(model)
            .and_then(|m| m.thinking_param())
            .unwrap_or(tars_config::ThinkingParam::Budget);
        let supports_off = model_supports_thinking_off(model);
        match param {
            tars_config::ThinkingParam::Budget => {
                let budget: Option<i64> = req.thinking.map(|mode| match mode {
                    tars_types::ThinkingMode::Off => 0,
                    tars_types::ThinkingMode::Auto => -1,
                    tars_types::ThinkingMode::Budget(b) => i64::from(b),
                });
                if let Some(budget) = budget.filter(|b| *b != 0 || supports_off) {
                    config.insert("thinkingConfig".into(), json!({ "thinkingBudget": budget }));
                }
            }
            tars_config::ThinkingParam::Level => {
                // A positive budget becomes "high", since 3.x has no numeric one. A thinking-only
                // model refuses "minimal" (HTTP 400 "Thinking level MINIMAL is not supported")
                // and left alone thinks at `medium` or `high`, so off asks for "low", the least
                // it accepts (tried on 3.1-pro-preview, 3.7-flash and 3.8-flash).
                let off_level = if supports_off { "minimal" } else { "low" };
                let level: Option<&str> = match req.thinking {
                    None | Some(tars_types::ThinkingMode::Auto) => None,
                    Some(tars_types::ThinkingMode::Off | tars_types::ThinkingMode::Budget(0)) => {
                        Some(off_level)
                    }
                    Some(tars_types::ThinkingMode::Budget(_)) => Some("high"),
                };
                if let Some(level) = level {
                    config.insert("thinkingConfig".into(), json!({ "thinkingLevel": level }));
                }
            }
        }

        if !config.is_empty() {
            body["generationConfig"] = Value::Object(config);
        }

        if let Some(sys) = &req.system {
            // A `systemInstruction` has no role field.
            body["systemInstruction"] = json!({
                "parts": [{"text": sys}],
            });
        }

        if !req.tools.is_empty() {
            body["tools"] = Self::translate_tools(&req.tools)?;
            let mode = match &req.tool_choice {
                tars_types::ToolChoice::Auto => json!({"mode": "AUTO"}),
                tars_types::ToolChoice::None => json!({"mode": "NONE"}),
                tars_types::ToolChoice::Required => json!({"mode": "ANY"}),
                tars_types::ToolChoice::Specific(name) => {
                    if name.is_empty() {
                        return Err(ProviderError::InvalidRequest(
                            RequestRefusal::EmptyToolChoiceName,
                        ));
                    }
                    if !req.tools.iter().any(|t| &t.name == name) {
                        return Err(ProviderError::InvalidRequest(
                            RequestRefusal::UnknownToolChoice { name: name.clone() },
                        ));
                    }
                    json!({
                        "mode": "ANY",
                        "allowed_function_names": [name],
                    })
                }
            };
            body["toolConfig"] = json!({"functionCallingConfig": mode});
        }

        for d in &req.cache_directives {
            if let tars_types::CacheDirective::UseExplicit { handle } = d {
                body["cachedContent"] = json!(handle.external_id);
                break;
            }
        }

        Ok(body)
    }

    fn parse_event(
        &self,
        raw: &SseEvent,
        buf: &mut StreamDecodeState,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        if raw.data.is_empty() {
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(&raw.data).map_err(|e| ProviderError::Parse {
            at: "gemini sse",
            line: None,
            problem: Box::new(ParseProblem::Json(e)),
            raw: Some(truncate(&raw.data, 200)),
        })?;

        let mut out = Vec::new();
        let candidates = match v.get("candidates").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => {
                // Only a `blockReason` says the prompt was blocked; `promptFeedback` can come with
                // nothing but safety ratings on a prompt that went through.
                if let Some(reason) = v
                    .pointer("/promptFeedback/blockReason")
                    .and_then(|s| s.as_str())
                {
                    return Err(ProviderError::ContentFiltered {
                        category: reason.to_string(),
                    });
                }
                return Ok(out);
            }
        };

        let model_version = v
            .get("modelVersion")
            .and_then(|s| s.as_str())
            .filter(|m| !m.is_empty());
        if let Some(model_version) = model_version
            && buf.take_started()
        {
            out.push(ChatEvent::started(model_version));
        }

        // Only the first candidate is read: a stream has one `Finished`, and the request never
        // asks for more than one.
        if let Some(cand) = candidates.first() {
            let parts = cand
                .pointer("/content/parts")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            for part in parts.into_iter() {
                let is_thought = part
                    .get("thought")
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false);
                if let Some(text) = part.get("text").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    if is_thought {
                        out.push(ChatEvent::ThinkingDelta {
                            text: text.to_string(),
                        });
                    } else {
                        out.push(ChatEvent::Delta {
                            text: text.to_string(),
                        });
                    }
                }
                if let Some(fc) = part.get("functionCall") {
                    // Gemini rejects a replayed call without it ("Function call is missing a
                    // thought_signature").
                    let thought_signature = part
                        .get("thoughtSignature")
                        .and_then(|s| s.as_str())
                        .map(str::to_string);
                    let name = fc
                        .get("name")
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| ProviderError::Parse {
                            at: "gemini functionCall",
                            line: None,
                            problem: Box::new(ParseProblem::Missing("functionCall.name")),
                            raw: Some(truncate(&raw.data, 200)),
                        })?
                        .to_string();
                    let args = fc
                        .get("args")
                        .cloned()
                        .unwrap_or(Value::Object(Default::default()));
                    // Numbered over the whole stream: one response can spread its calls over
                    // several events, and each needs its own index and id.
                    let idx = buf.ended_calls();
                    let call_id = format!("{name}@gemini-call-{idx}");
                    out.push(ChatEvent::ToolCallStart {
                        index: idx,
                        id: call_id.clone(),
                        name: name.clone(),
                    });
                    // Gemini sends whole arguments, so a call starts and ends in one event, and an
                    // output limit cannot cut one.
                    let args_str = args.to_string();
                    out.push(ChatEvent::ToolCallArgsDelta {
                        index: idx,
                        args_delta: args_str.clone(),
                    });
                    buf.on_start(idx, call_id.clone(), name);
                    buf.on_delta(idx, &args_str);
                    let call = buf.finalize(idx, None, thought_signature)?;
                    out.push(ChatEvent::ToolCallEnd {
                        index: idx,
                        id: call.id,
                        parsed_args: call.arguments,
                        thought_signature: call.thought_signature,
                    });
                }
            }

            if let Some(reason_str) = cand.get("finishReason").and_then(|r| r.as_str()) {
                let usage = v
                    .get("usageMetadata")
                    .and_then(|u| u.as_object())
                    .map(parse_usage);
                // Gemini stops with `STOP` after a tool call too; a call anywhere in the stream
                // turns it into `ToolUse`, as other providers report.
                let stop = match map_stop_reason(reason_str) {
                    StopReason::EndTurn if buf.ended_calls() > 0 => StopReason::ToolUse,
                    stop => stop,
                };
                out.push(ChatEvent::Finished {
                    stop_reason: stop,
                    usage,
                });
                buf.mark_finished();
            }
        }

        Ok(out)
    }

    fn classify_error(
        &self,
        status: StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> ProviderError {
        let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let message = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| truncate(body, 300));

        response_error(
            status.as_u16(),
            message,
            crate::provider::http_base::parse_retry_after(headers),
        )
        .stated(crate::provider::http_base::stated_by_headers(headers))
    }

    fn extras(&self) -> &HttpOverrides {
        &self.extras
    }
}

/// Returns the error of a Gemini error response from its status and message; `retry_after` is
/// its `Retry-After`.
fn response_error(status: u16, message: String, retry_after: Option<Duration>) -> ProviderError {
    let lower = message.to_lowercase();
    match status {
        // Gemini's message is prose without counts.
        400 if lower.contains("token") && lower.contains("limit") => {
            ProviderError::ContextTooLong {
                limit: None,
                requested: None,
                message,
            }
        }
        400 => ProviderError::InvalidRequest(RequestRefusal::Provider(ProviderRefusal {
            status: Some(400),
            message,
        })),
        _ => status_error(status, message, retry_after),
    }
}

/// Returns a tool result's text blocks joined by newlines; an image cannot travel in
/// `functionResponse` as tars sends it, so it is refused rather than dropped.
fn tool_result_text(content: &[ContentBlock]) -> Result<String, ProviderError> {
    content
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => Ok(text.as_str()),
            ContentBlock::Image { .. } => Err(ProviderError::InvalidRequest(
                RequestRefusal::Unsupported(Feature::ImageInToolResult),
            )),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|texts| texts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::backends::gemini::mapping::map_stop_reason as map_stop_reason_test;

    fn adapter() -> GeminiAdapter {
        GeminiAdapter::new(
            "https://generativelanguage.googleapis.com".to_string(),
            HttpOverrides::default(),
        )
    }

    #[test]
    fn translates_assistant_to_model_role() {
        let m = Message::assistant_text("hello");
        let v = GeminiAdapter::translate_message(&m).unwrap();
        assert_eq!(v["role"], "model");
        assert_eq!(v["parts"][0]["text"], "hello");
    }

    #[test]
    fn assistant_tool_call_echoes_thought_signature_on_replay() {
        let tc = tars_types::ToolCall::new(
            "id@1",
            "fs.read_file",
            serde_json::json!({"path": "a"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .with_thought_signature(Some("SIG-abc".to_string()));
        let m = tars_types::Message::Assistant {
            content: vec![],
            tool_calls: vec![tc],
        };
        let v = GeminiAdapter::translate_message(&m).unwrap();
        let part = &v["parts"][0];
        assert_eq!(part["functionCall"]["name"], "fs.read_file");
        assert_eq!(part["thoughtSignature"], "SIG-abc");

        let plain = tars_types::ToolCall::new(
            "id@2",
            "fs.read_file",
            serde_json::json!({"path": "a"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let mv = GeminiAdapter::translate_message(&tars_types::Message::Assistant {
            content: vec![],
            tool_calls: vec![plain],
        })
        .unwrap();
        assert!(mv["parts"][0].get("thoughtSignature").is_none());
    }

    #[test]
    fn translates_user_to_user_role() {
        let m = Message::user_text("hi");
        let v = GeminiAdapter::translate_message(&m).unwrap();
        assert_eq!(v["role"], "user");
    }

    #[test]
    fn tool_message_without_name_in_id_is_invalid_request() {
        let m = Message::Tool {
            tool_call_id: "abc-123".into(),
            content: vec![tars_types::ContentBlock::text("ok")],
            is_error: false,
        };
        let err = GeminiAdapter::translate_message(&m).unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn tool_message_extracts_name_from_id_and_encodes_error() {
        let m = Message::Tool {
            tool_call_id: "search@call-1".into(),
            content: vec![tars_types::ContentBlock::text("boom")],
            is_error: true,
        };
        let v = GeminiAdapter::translate_message(&m).unwrap();
        assert_eq!(v["parts"][0]["functionResponse"]["name"], "search");
        assert_eq!(
            v["parts"][0]["functionResponse"]["response"]["error"],
            "boom"
        );
    }

    #[test]
    fn empty_messages_rejected_early() {
        let mut req = ChatRequest::user("hi");
        req.messages.clear();
        let err = adapter()
            .translate_request(&req, "gemini-2.5-flash")
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn tool_choice_specific_unknown_name_rejected() {
        let mut req = ChatRequest::user("hi");
        req.tools = vec![tars_types::ToolSpec {
            name: "real_tool".into(),
            description: "".into(),
            input_schema: tars_types::JsonSchema::strict(
                "X",
                tars_types::json_object! {"type":"object"},
            ),
        }];
        req.tool_choice = tars_types::ToolChoice::Specific("ghost".into());
        let err = adapter()
            .translate_request(&req, "gemini-2.5-flash")
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn system_instruction_has_no_role_field() {
        let req = ChatRequest::user("hi").with_system("be brief");
        let body = adapter()
            .translate_request(&req, "gemini-2.5-flash")
            .unwrap();
        assert!(body["systemInstruction"].get("role").is_none());
    }

    fn tool_result(content: Vec<ContentBlock>) -> ChatRequest {
        let mut req = ChatRequest::user("hi");
        req.messages.push(Message::Tool {
            tool_call_id: "look@gemini-call-0".into(),
            content,
            is_error: false,
        });
        req
    }

    #[test]
    fn every_text_block_of_a_tool_result_is_sent() {
        let req = tool_result(vec![
            ContentBlock::text("first"),
            ContentBlock::text("second"),
        ]);
        let body = adapter()
            .translate_request(&req, "gemini-2.5-flash")
            .unwrap();
        let output = &body["contents"][1]["parts"][0]["functionResponse"]["response"]["output"];
        assert_eq!(output, "first\nsecond");
    }

    #[test]
    fn an_image_in_a_tool_result_is_refused_not_dropped() {
        let req = tool_result(vec![
            ContentBlock::text("see"),
            ContentBlock::Image {
                mime: "image/png".into(),
                data: ImageData::Base64("AAAA".into()),
            },
        ]);
        let err = adapter()
            .translate_request(&req, "gemini-2.5-flash")
            .unwrap_err();
        assert!(
            matches!(
                err,
                ProviderError::InvalidRequest(RequestRefusal::Unsupported(
                    Feature::ImageInToolResult
                ))
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_stream_starts_once_though_every_chunk_names_the_model() {
        let a = GeminiAdapter::new("https://x".into(), HttpOverrides::default());
        let mut buf = StreamDecodeState::new();
        let chunk = SseEvent {
            event: "message".into(),
            data: json!({"modelVersion": "gemini-x", "candidates": [{"content": {"parts": [{"text": "a"}]}}]}).to_string(),
        };
        let starts = (0..3)
            .flat_map(|_| a.parse_event(&chunk, &mut buf).unwrap())
            .filter(|e| matches!(e, ChatEvent::Started { .. }))
            .count();
        assert_eq!(starts, 1);
    }

    #[test]
    fn parse_event_uses_only_first_candidate() {
        let mut buf = StreamDecodeState::new();
        let raw = SseEvent {
            event: "message".into(),
            data: r#"{"candidates":[
                {"content":{"parts":[{"text":"a"}]},"finishReason":"STOP"},
                {"content":{"parts":[{"text":"b"}]},"finishReason":"STOP"}
            ]}"#
            .into(),
        };
        let events = adapter().parse_event(&raw, &mut buf).unwrap();
        let finished = events.iter().filter(|e| e.is_terminal()).count();
        assert_eq!(finished, 1, "exactly one Finished event expected");
        let has_b = events
            .iter()
            .any(|e| matches!(e, ChatEvent::Delta { text } if text == "b"));
        assert!(!has_b, "second candidate must be ignored");
    }

    #[test]
    fn parse_event_tool_call_id_embeds_name() {
        let mut buf = StreamDecodeState::new();
        let raw = SseEvent {
            event: "message".into(),
            data: r#"{"candidates":[
                {"content":{"parts":[
                    {"text":"thinking..."},
                    {"functionCall":{"name":"search","args":{"q":"x"}}}
                ]},"finishReason":"STOP"}
            ]}"#
            .into(),
        };
        let events = adapter().parse_event(&raw, &mut buf).unwrap();
        let start = events.iter().find_map(|e| match e {
            ChatEvent::ToolCallStart { index, id, name } => {
                Some((*index, id.clone(), name.clone()))
            }
            _ => None,
        });
        let (idx, id, name) = start.expect("ToolCallStart present");
        assert_eq!(idx, 0, "a text part does not take a call index");
        assert_eq!(name, "search");
        assert!(id.starts_with("search@"), "id must embed name: {id}");
        let stop = events.iter().find_map(|e| match e {
            ChatEvent::Finished { stop_reason, .. } => Some(*stop_reason),
            _ => None,
        });
        assert_eq!(stop, Some(StopReason::ToolUse));
    }

    #[test]
    fn calls_in_two_events_get_distinct_indexes_and_a_later_stop_is_tool_use() {
        let mut buf = StreamDecodeState::new();
        let event = |data: &str| SseEvent {
            event: "message".into(),
            data: data.into(),
        };
        let first = adapter()
            .parse_event(
                &event(r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"a","args":{}}}]}}]}"#),
                &mut buf,
            )
            .unwrap();
        let second = adapter()
            .parse_event(
                &event(r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"a","args":{}}}]}}]}"#),
                &mut buf,
            )
            .unwrap();
        let stop = adapter()
            .parse_event(
                &event(r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}]}"#),
                &mut buf,
            )
            .unwrap();
        let start = |events: &[ChatEvent]| {
            events.iter().find_map(|e| match e {
                ChatEvent::ToolCallStart { index, id, .. } => Some((*index, id.clone())),
                _ => None,
            })
        };
        assert_eq!(start(&first), Some((0, "a@gemini-call-0".into())));
        assert_eq!(start(&second), Some((1, "a@gemini-call-1".into())));
        assert!(matches!(
            stop.last(),
            Some(ChatEvent::Finished {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[test]
    fn a_stop_with_no_call_in_the_stream_is_end_turn() {
        let mut buf = StreamDecodeState::new();
        let raw = SseEvent {
            event: "message".into(),
            data: r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}]}"#
                .into(),
        };
        let events = adapter().parse_event(&raw, &mut buf).unwrap();
        assert!(matches!(
            events.last(),
            Some(ChatEvent::Finished {
                stop_reason: StopReason::EndTurn,
                ..
            })
        ));
    }

    #[test]
    fn system_promotes_to_system_instruction() {
        let req = ChatRequest::user("hi").with_system("be brief");
        let body = adapter()
            .translate_request(&req, "gemini-2.5-flash")
            .unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        assert!(body["contents"].is_array());
    }

    #[test]
    fn structured_output_sets_response_schema() {
        let mut req = ChatRequest::user("json please");
        req.structured_output = Some(tars_types::JsonSchema::strict(
            "Resp",
            tars_types::json_object! {"type":"object"},
        ));
        let body = adapter()
            .translate_request(&req, "gemini-2.5-flash")
            .unwrap();
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert!(body["generationConfig"]["responseSchema"].is_object());
    }

    #[test]
    fn a_request_that_says_nothing_about_thinking_sends_no_thinking_config() {
        let req = ChatRequest::user("hi");
        for model in ["gemini-2.5-flash", "gemini-3.6-flash", "gemini-3.8-flash"] {
            let body = adapter().translate_request(&req, model).unwrap();
            assert!(
                body["generationConfig"]["thinkingConfig"].is_null(),
                "{model}: {body}"
            );
        }
    }

    #[test]
    fn thinking_off_omits_config_for_thinking_only_model() {
        let mut req = ChatRequest::user("hi");
        req.thinking = Some(tars_types::ThinkingMode::Off);
        let body = adapter().translate_request(&req, "gemini-2.5-pro").unwrap();
        assert!(
            body["generationConfig"]["thinkingConfig"].is_null(),
            "gemini-2.5-pro is thinking-only; thinkingConfig must be omitted, got: {body}"
        );
    }

    #[test]
    fn thinking_off_asks_low_on_a_level_model_that_rejects_minimal() {
        let mut req = ChatRequest::user("hi");
        req.thinking = Some(tars_types::ThinkingMode::Off);
        for model in ["gemini-3.8-flash", "gemini-3.1-pro-preview"] {
            let body = adapter().translate_request(&req, model).unwrap();
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "low",
                "{model}: {body}"
            );
        }
        let body = adapter()
            .translate_request(&req, "gemini-3.6-flash")
            .unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "minimal"
        );
    }

    #[test]
    fn thinking_off_sets_zero_budget_for_flash() {
        let mut req = ChatRequest::user("hi");
        req.thinking = Some(tars_types::ThinkingMode::Off);
        let body = adapter()
            .translate_request(&req, "gemini-2.5-flash")
            .unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
    }

    #[test]
    fn model_supports_thinking_off_predicate() {
        assert!(model_supports_thinking_off("gemini-2.5-flash"));
        assert!(model_supports_thinking_off("gemini-3.5-flash"));
        assert!(model_supports_thinking_off("gemini-3.1-flash-lite"));
        assert!(!model_supports_thinking_off("gemini-2.5-pro"));
        assert!(!model_supports_thinking_off("gemini-3.1-pro-preview"));
        assert!(!model_supports_thinking_off("gemini-flash-latest"));
        assert!(!model_supports_thinking_off("gemini-pro-latest"));
    }

    #[test]
    fn safety_block_returns_content_filtered() {
        let mut buf = StreamDecodeState::new();
        let raw = SseEvent {
            event: "message".into(),
            data: r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#.into(),
        };
        let err = adapter().parse_event(&raw, &mut buf).unwrap_err();
        assert!(matches!(
            err,
            ProviderError::ContentFiltered { ref category } if category == "SAFETY"
        ));
    }

    #[test]
    fn prompt_feedback_without_a_block_reason_is_not_a_block() {
        let mut buf = StreamDecodeState::new();
        let raw = SseEvent {
            event: "message".into(),
            data:
                r#"{"promptFeedback":{"safetyRatings":[]},"usageMetadata":{"promptTokenCount":4}}"#
                    .into(),
        };
        assert!(adapter().parse_event(&raw, &mut buf).unwrap().is_empty());
    }

    #[test]
    fn a_function_call_without_a_name_is_a_parse_error_carrying_the_event() {
        let mut buf = StreamDecodeState::new();
        let raw = SseEvent {
            event: "message".into(),
            data: r#"{"candidates":[{"content":{"parts":[{"functionCall":{"args":{}}}]}}]}"#.into(),
        };
        let err = adapter().parse_event(&raw, &mut buf).unwrap_err();
        assert!(
            matches!(
                &err,
                ProviderError::Parse { problem, raw: Some(_), .. }
                    if matches!(**problem, ParseProblem::Missing("functionCall.name"))
            ),
            "{err:?}"
        );
    }

    #[test]
    fn map_stop_reasons() {
        assert_eq!(map_stop_reason_test("STOP"), StopReason::EndTurn);
        assert_eq!(map_stop_reason_test("MAX_TOKENS"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason_test("SAFETY"), StopReason::ContentFilter);
        assert_eq!(
            map_stop_reason_test("RECITATION"),
            StopReason::ContentFilter
        );
        assert_eq!(map_stop_reason_test("WHATEVER"), StopReason::Other);
    }

    #[test]
    fn classify_400_token_limit_is_context_too_long() {
        let a = adapter();
        let body = r#"{"error":{"message":"input token limit exceeded"}}"#;
        let err = a.classify_error(
            StatusCode::BAD_REQUEST,
            &reqwest::header::HeaderMap::new(),
            body,
        );
        match err {
            ProviderError::ContextTooLong {
                limit,
                requested,
                message,
            } => {
                assert_eq!((limit, requested), (None, None));
                assert_eq!(message, "input token limit exceeded");
            }
            other => panic!("expected ContextTooLong, got {other:?}"),
        }
    }

    #[test]
    fn tool_parameters_drop_gemini_unsupported_schema_keys() {
        let spec = tars_types::ToolSpec::new(
            "fs.read_file",
            "read a file",
            tars_types::JsonSchema::strict(
                "Args",
                tars_types::json_object! {
                    "type": "object",
                    "additionalProperties": false,
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                },
            ),
        )
        .unwrap();
        let out = GeminiAdapter::translate_tools(std::slice::from_ref(&spec)).unwrap();
        let text = serde_json::to_string(&out).unwrap();
        assert!(
            !text.contains("additionalProperties"),
            "Gemini rejects additionalProperties in tool params: {text}"
        );
        assert!(
            !text.contains("$schema"),
            "draft meta must be stripped: {text}"
        );
        assert!(text.contains("path"));
    }

    #[test]
    fn a_one_character_api_key_becomes_a_sensitive_header_and_an_empty_one_is_refused() {
        let value = api_key_header(&ResolvedAuth::ApiKey("k".into())).unwrap();
        assert_eq!(value, "k");
        assert!(value.is_sensitive());

        let err = api_key_header(&ResolvedAuth::ApiKey(String::new())).unwrap_err();
        assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
    }

    #[test]
    fn a_bearer_token_or_no_credential_is_refused() {
        for auth in [ResolvedAuth::Bearer("tok".into()), ResolvedAuth::None] {
            let err = api_key_header(&auth).unwrap_err();
            assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
        }
    }

    #[test]
    fn a_key_that_is_no_header_value_is_refused_without_echoing_it() {
        let err = api_key_header(&ResolvedAuth::ApiKey("AIza\nSECRET".into())).unwrap_err();
        assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
        assert!(!format!("{err} {err:?}").contains("SECRET"));
    }

    #[test]
    fn the_url_carries_no_key() {
        let url = adapter().build_url("gemini-2.5-pro").unwrap();
        assert_eq!(url.query(), Some("alt=sse"));
    }

    #[test]
    fn a_5xx_other_than_503_is_a_server_error_and_a_408_a_request_timeout() {
        use tars_types::LlmErrorKind as K;
        let kind = |status| response_error(status, "m".into(), None).kind();
        assert_eq!(kind(500), K::ServerError);
        assert_eq!(kind(502), K::ServerError);
        assert_eq!(kind(503), K::ModelOverloaded);
        assert_eq!(kind(504), K::ServerError);
        assert_eq!(kind(408), K::RequestTimeout);
        assert_eq!(kind(409), K::InvalidRequest);
    }

    #[test]
    fn x_should_retry_decides_the_class_over_the_status() {
        use tars_types::ErrorClass;
        let classify = |status, should_retry: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("x-should-retry", should_retry.parse().unwrap());
            adapter().classify_error(status, &headers, "{}").class()
        };
        assert_eq!(
            classify(StatusCode::CONFLICT, "true"),
            ErrorClass::Transient
        );
        assert_eq!(
            classify(StatusCode::BAD_GATEWAY, "false"),
            ErrorClass::Permanent
        );
    }
}
