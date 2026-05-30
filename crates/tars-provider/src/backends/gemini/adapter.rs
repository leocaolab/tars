//! Protocol-translation layer for the Gemini backend. Owns request
//! body construction (`translate_request`), SSE event parsing
//! (`parse_event`), error classification, and the two `HttpAdapter`
//! impls (one pure for testability, one composed with the resolved
//! API key for production use).

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use url::Url;

use tars_types::{
    ChatEvent, ChatRequest, ContentBlock, ImageData, Message, ProviderError, StopReason,
};

use crate::auth::ResolvedAuth;
use crate::http_base::{HttpAdapter, HttpProviderExtras, SseEvent};
use crate::tool_buffer::ToolCallBuffer;

use super::mapping::{map_stop_reason, parse_usage, truncate, urlencoding};

const API_VERSION: &str = "v1beta";

/// Resolved auth in the shape this backend uses internally. Gemini's
/// only supported variant is API key; bearer (ADC) lands in
/// [`super::provider::GeminiProvider::stream`] as an explicit error.
pub(super) enum ResolvedAuthWithKey {
    Key(String),
}

/// Pure adapter without the API key. The base_url + extras are
/// enough to translate requests and parse events; tests can construct
/// this directly without resolving auth.
pub struct GeminiAdapter {
    pub(super) base_url: String,
    pub(super) extras: HttpProviderExtras,
}

/// Adapter composed with a resolved API key — produced per request by
/// the provider's `stream` path. Delegates everything except URL
/// construction to the inner [`GeminiAdapter`].
pub(super) struct GeminiAdapterWithKey {
    pub(super) inner: Arc<GeminiAdapter>,
    pub(super) key: String,
}

#[async_trait]
impl HttpAdapter for GeminiAdapterWithKey {
    fn build_url(&self, model: &str) -> Result<Url, ProviderError> {
        // streamGenerateContent + alt=sse for SSE framing.
        let trimmed = self.inner.base_url.trim_end_matches('/');
        Url::parse(&format!(
            "{trimmed}/{API_VERSION}/models/{model}:streamGenerateContent?alt=sse&key={}",
            urlencoding(&self.key)
        ))
        .map_err(|e| {
            ProviderError::Internal(format!(
                "bad gemini url for model '{model}' (base_url='{trimmed}'): {e}"
            ))
        })
    }

    fn build_headers(&self, _auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(h)
    }

    fn translate_request(&self, req: &ChatRequest) -> Result<Value, ProviderError> {
        self.inner.translate_request(req)
    }

    fn parse_event(
        &self,
        raw: &SseEvent,
        buf: &mut ToolCallBuffer,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        self.inner.parse_event(raw, buf)
    }

    fn classify_error(
        &self,
        status: StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> ProviderError {
        self.inner.classify_error(status, headers, body)
    }

    fn extras(&self) -> &HttpProviderExtras {
        &self.inner.extras
    }
}

impl GeminiAdapter {
    pub(super) fn new(base_url: String, extras: HttpProviderExtras) -> Self {
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
                    parts.push(json!({
                        "functionCall": {
                            "name": tc.name,
                            "args": tc.arguments,
                        }
                    }));
                }
                Ok(json!({"role": "model", "parts": parts}))
            }
            Message::Tool {
                tool_call_id,
                content,
                is_error,
            } => {
                // Gemini's functionResponse needs the function's *name*, but
                // tars's canonical Message::Tool only carries the
                // `tool_call_id`. Convention: callers targeting Gemini must
                // encode the function name as `<name>@<id>` so this layer
                // can recover it. Reject with a clear, actionable error
                // when the convention is missing — Gemini's own 400 is
                // cryptic ("functionResponse.name is required").
                let name = match tool_call_id.split_once('@') {
                    Some((n, _)) if !n.is_empty() => n,
                    _ => {
                        return Err(ProviderError::InvalidRequest(format!(
                            "Gemini tool result requires `<name>@<id>` tool_call_id; got `{tool_call_id}`"
                        )));
                    }
                };
                let text = content
                    .first()
                    .and_then(|b| b.as_text())
                    .unwrap_or("")
                    .to_string();
                // Encode failure into the response object so the model
                // doesn't mistake a failed call for a successful one.
                // Gemini doesn't have a dedicated `is_error` flag for
                // functionResponse, so we surface it inside `response`.
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
                // Gemini has no "system" message role — system text belongs
                // in `systemInstruction`, which `translate_request` builds
                // from `ChatRequest.system` (see lines below). Silently
                // relabelling a system message as "user" would change how
                // the model weights the instruction, so reject with an
                // actionable error (mirrors the Tool-message handling above)
                // and steer callers to the dedicated `system` field.
                Err(ProviderError::InvalidRequest(
                    "Gemini does not support a `system` message role; \
                     put system text in `ChatRequest.system` instead"
                        .into(),
                ))
            }
        }
    }

    fn translate_tools(tools: &[tars_types::ToolSpec]) -> Value {
        let declarations: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema.schema,
                })
            })
            .collect();
        json!([{"functionDeclarations": declarations}])
    }
}

#[async_trait]
impl HttpAdapter for GeminiAdapter {
    fn build_url(&self, model: &str) -> Result<Url, ProviderError> {
        // Used only directly in tests; production path goes via
        // `GeminiAdapterWithKey`.
        let trimmed = self.base_url.trim_end_matches('/');
        Url::parse(&format!(
            "{trimmed}/{API_VERSION}/models/{model}:streamGenerateContent?alt=sse"
        ))
        .map_err(|e| {
            ProviderError::Internal(format!(
                "bad gemini url for model '{model}' (base_url='{trimmed}'): {e}"
            ))
        })
    }

    fn build_headers(&self, _auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(h)
    }

    fn translate_request(&self, req: &ChatRequest) -> Result<Value, ProviderError> {
        let _ = req.model.explicit().ok_or_else(|| {
            ProviderError::InvalidRequest(format!("model must be explicit, got: {:?}", req.model))
        })?;

        if req.messages.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "messages cannot be empty".into(),
            ));
        }

        let contents: Vec<Value> = req
            .messages
            .iter()
            .map(Self::translate_message)
            .collect::<Result<_, _>>()?;

        let mut body = json!({
            "contents": contents,
        });

        let mut config = json!({});
        if let Some(max) = req.max_output_tokens {
            config["maxOutputTokens"] = json!(max);
        }
        if let Some(t) = req.temperature {
            config["temperature"] = json!(t);
        }
        if !req.stop_sequences.is_empty() {
            config["stopSequences"] = json!(req.stop_sequences);
        }
        if let Some(seed) = req.seed {
            config["seed"] = json!(seed);
        }

        if let Some(schema) = &req.structured_output {
            config["responseMimeType"] = json!("application/json");
            config["responseSchema"] = schema.schema.clone();
        }

        // Thinking config (Gemini 2.5+ family).
        match req.thinking {
            tars_types::ThinkingMode::Off => {
                config["thinkingConfig"] = json!({"thinkingBudget": 0});
            }
            tars_types::ThinkingMode::Auto => {
                config["thinkingConfig"] = json!({"thinkingBudget": -1});
            }
            tars_types::ThinkingMode::Budget(b) => {
                config["thinkingConfig"] = json!({"thinkingBudget": b});
            }
        }

        if config.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            body["generationConfig"] = config;
        }

        if let Some(sys) = &req.system {
            // Gemini's `systemInstruction` is a Content but role is
            // implicit ("system") — passing role: "user" here is
            // misleading and contradicts the file-level header doc.
            body["systemInstruction"] = json!({
                "parts": [{"text": sys}],
            });
        }

        if !req.tools.is_empty() {
            body["tools"] = Self::translate_tools(&req.tools);
            // tool_choice → toolConfig.functionCallingConfig.mode
            let mode = match &req.tool_choice {
                tars_types::ToolChoice::Auto => json!({"mode": "AUTO"}),
                tars_types::ToolChoice::None => json!({"mode": "NONE"}),
                tars_types::ToolChoice::Required => json!({"mode": "ANY"}),
                tars_types::ToolChoice::Specific(name) => {
                    if name.is_empty() {
                        return Err(ProviderError::InvalidRequest(
                            "ToolChoice::Specific name is empty".into(),
                        ));
                    }
                    if !req.tools.iter().any(|t| &t.name == name) {
                        return Err(ProviderError::InvalidRequest(format!(
                            "ToolChoice::Specific(`{name}`) not present in tools list"
                        )));
                    }
                    json!({
                        "mode": "ANY",
                        "allowed_function_names": [name],
                    })
                }
            };
            body["toolConfig"] = json!({"functionCallingConfig": mode});
        }

        // Cache directive → cachedContent reference.
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
        buf: &mut ToolCallBuffer,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        if raw.data.is_empty() {
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(&raw.data).map_err(|e| {
            ProviderError::Parse(format!(
                "gemini sse: {e} (raw: {})",
                truncate(&raw.data, 200)
            ))
        })?;

        let mut out = Vec::new();
        let model_version = v
            .get("modelVersion")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();

        // Safety filter: candidates is None / missing.
        let candidates = match v.get("candidates").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => {
                // promptFeedback indicates the prompt itself was blocked.
                if let Some(fb) = v.get("promptFeedback") {
                    let category = fb
                        .get("blockReason")
                        .and_then(|s| s.as_str())
                        .unwrap_or("safety")
                        .to_string();
                    return Err(ProviderError::ContentFiltered { category });
                }
                // Otherwise nothing to do.
                return Ok(out);
            }
        };

        if !model_version.is_empty() && out.is_empty() {
            out.push(ChatEvent::started(model_version));
        }

        // ChatEvent contract: exactly one Finished event per response.
        // Gemini may return >1 candidate when n>1 sampling is requested,
        // but our request never asks for that — and emitting one
        // Finished per candidate would break downstream consumers.
        // Process only candidates[0]; ignore the rest.
        if let Some(cand) = candidates.first() {
            // Track whether this candidate contained any tool call so we
            // can override Gemini's generic `STOP` finishReason → ToolUse
            // when appropriate. Gemini doesn't have a dedicated
            // tool-use stop reason the way OpenAI/Anthropic do; without
            // this normalization the cross-provider conformance suite
            // sees ToolUse on those two but EndTurn on Gemini for the
            // same logical "model wants to call a function" outcome.
            let mut had_function_call = false;

            // Parts inside content.
            let parts = cand
                .pointer("/content/parts")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            // Tool-call index is a counter over functionCall parts only —
            // not the part position in the array. Mixing text parts and
            // functionCall parts would otherwise produce non-sequential
            // tool indices that downstream tool-buffer logic expects to
            // be 0..N.
            let mut fc_idx: usize = 0;
            for part in parts.into_iter() {
                let is_thought = part
                    .get("thought")
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false);
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
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
                }
                if let Some(fc) = part.get("functionCall") {
                    had_function_call = true;
                    let name = fc
                        .get("name")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = fc
                        .get("args")
                        .cloned()
                        .unwrap_or(Value::Object(Default::default()));
                    // Embed the function name in the id so that downstream
                    // Tool messages can recover it via the `<name>@<id>`
                    // convention required by `translate_message`.
                    let call_id = format!("{name}@gemini-call-{fc_idx}");
                    let idx = fc_idx;
                    fc_idx += 1;
                    out.push(ChatEvent::ToolCallStart {
                        index: idx,
                        id: call_id.clone(),
                        name: name.clone(),
                    });
                    // Gemini gives args as a parsed object — feed it as a
                    // single delta containing the JSON, then finalize.
                    let args_str = args.to_string();
                    out.push(ChatEvent::ToolCallArgsDelta {
                        index: idx,
                        args_delta: args_str.clone(),
                    });
                    buf.on_start(idx, call_id.clone(), name);
                    buf.on_delta(idx, &args_str);
                    let (id, _name, parsed) = buf.finalize(idx)?;
                    out.push(ChatEvent::ToolCallEnd {
                        index: idx,
                        id,
                        parsed_args: parsed,
                    });
                }
            }

            if let Some(reason_str) = cand.get("finishReason").and_then(|r| r.as_str()) {
                let usage = v
                    .get("usageMetadata")
                    .and_then(|u| u.as_object())
                    .map(parse_usage)
                    .unwrap_or_default();
                let mut stop = map_stop_reason(reason_str);
                if had_function_call && matches!(stop, StopReason::EndTurn) {
                    // Cross-provider normalization (Doc 01 §8): when the
                    // model emitted a tool call, the upstream signal is
                    // "caller, please run this and continue" — same as
                    // OpenAI's `tool_calls` and Anthropic's `tool_use`.
                    // Surface that instead of the wire-level STOP.
                    stop = StopReason::ToolUse;
                }
                out.push(ChatEvent::Finished {
                    stop_reason: stop,
                    usage,
                });
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

        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderError::Auth(message),
            StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited {
                retry_after: crate::http_base::parse_retry_after(headers),
            },
            StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => {
                ProviderError::ModelOverloaded
            }
            StatusCode::BAD_REQUEST => {
                let lower = message.to_lowercase();
                if lower.contains("token") && lower.contains("limit") {
                    // Gemini's 400 body does not reliably carry machine-
                    // readable token counts (the message is free-form
                    // prose), so we cannot populate these fields. `0`/`0`
                    // is a documented sentinel meaning "unknown for this
                    // provider"; callers must rely on `message`/the error
                    // kind, not these numbers, for Gemini context errors.
                    ProviderError::ContextTooLong {
                        limit: 0,
                        requested: 0,
                    }
                } else {
                    ProviderError::InvalidRequest(message)
                }
            }
            s if s.is_server_error() => ProviderError::Internal(format!("status {s}: {message}")),
            _ => ProviderError::InvalidRequest(format!("status {status}: {message}")),
        }
    }

    fn extras(&self) -> &HttpProviderExtras {
        &self.extras
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::gemini::mapping::map_stop_reason as map_stop_reason_test;
    use crate::backends::gemini::mapping::urlencoding as urlencoding_test;

    fn adapter() -> GeminiAdapter {
        GeminiAdapter::new(
            "https://generativelanguage.googleapis.com".to_string(),
            HttpProviderExtras::default(),
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
        let mut req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "hi",
        );
        req.messages.clear();
        let err = adapter().translate_request(&req).unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn tool_choice_specific_unknown_name_rejected() {
        let mut req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "hi",
        );
        req.tools = vec![tars_types::ToolSpec {
            name: "real_tool".into(),
            description: "".into(),
            input_schema: tars_types::JsonSchema::strict("X", serde_json::json!({"type":"object"})),
        }];
        req.tool_choice = tars_types::ToolChoice::Specific("ghost".into());
        let err = adapter().translate_request(&req).unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn system_instruction_has_no_role_field() {
        let req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "hi",
        )
        .with_system("be brief");
        let body = adapter().translate_request(&req).unwrap();
        assert!(body["systemInstruction"].get("role").is_none());
    }

    #[test]
    fn parse_event_uses_only_first_candidate() {
        let mut buf = ToolCallBuffer::new();
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
        let mut buf = ToolCallBuffer::new();
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
        assert_eq!(idx, 0, "function-call counter resets across mixed parts");
        assert_eq!(name, "search");
        assert!(id.starts_with("search@"), "id must embed name: {id}");
        let stop = events.iter().find_map(|e| match e {
            ChatEvent::Finished { stop_reason, .. } => Some(*stop_reason),
            _ => None,
        });
        assert_eq!(stop, Some(StopReason::ToolUse));
    }

    #[test]
    fn system_promotes_to_system_instruction() {
        let req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "hi",
        )
        .with_system("be brief");
        let body = adapter().translate_request(&req).unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        assert!(body["contents"].is_array());
    }

    #[test]
    fn structured_output_sets_response_schema() {
        let mut req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "json please",
        );
        req.structured_output = Some(tars_types::JsonSchema::strict(
            "Resp",
            serde_json::json!({"type":"object"}),
        ));
        let body = adapter().translate_request(&req).unwrap();
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert!(body["generationConfig"]["responseSchema"].is_object());
    }

    #[test]
    fn thinking_off_sets_zero_budget() {
        let req = ChatRequest::user(
            tars_types::ModelHint::Explicit("gemini-2.5-pro".into()),
            "hi",
        );
        let body = adapter().translate_request(&req).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
    }

    #[test]
    fn safety_block_returns_content_filtered() {
        let mut buf = ToolCallBuffer::new();
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
    fn map_stop_reasons() {
        assert_eq!(map_stop_reason_test("STOP"), StopReason::EndTurn);
        assert_eq!(map_stop_reason_test("MAX_TOKENS"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason_test("SAFETY"), StopReason::ContentFilter);
        assert_eq!(map_stop_reason_test("RECITATION"), StopReason::ContentFilter);
        assert_eq!(map_stop_reason_test("WHATEVER"), StopReason::Other);
    }

    #[test]
    fn url_encode_handles_special_chars() {
        assert_eq!(urlencoding_test("abc-123_X"), "abc-123_X");
        assert_eq!(urlencoding_test("a b"), "a%20b");
        assert_eq!(urlencoding_test("a/b"), "a%2Fb");
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
        assert!(matches!(err, ProviderError::ContextTooLong { .. }));
    }
}
