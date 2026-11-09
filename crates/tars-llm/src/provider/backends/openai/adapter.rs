//! Translates requests into OpenAI's chat-completions body and its stream events back into
//! [`ChatEvent`]s. Tool names travel with `.` written as `-`, which OpenAI's name pattern
//! `^[a-zA-Z0-9_-]+$` requires; the events carry the name as sent, and the provider reads it
//! back against the request's own tools ([`tool_name_from_wire`]).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use url::Url;

use crate::ProviderError;
use crate::provider::error::{
    CredentialError, ParseProblem, ProviderRefusal, RequestRefusal, status_error,
};
use tars_types::{
    ChatEvent, ChatRequest, ContentBlock, Message, ProviderProfile, StructuredOutputMode,
};

use crate::provider::auth::ResolvedAuth;
use crate::provider::http_base::{HttpAdapter, HttpOverrides, SseEvent, parse_url, truncate};
use crate::provider::tool_buffer::{StreamDecodeState, StreamEnd};

use super::dialect::{OpenAiDialect, StandardDialect};
use super::mapping::{map_finish_reason, message_events, track_tool_calls};

/// Names the model prefixes that take `max_completion_tokens` instead of `max_tokens`.
const NEW_TOKENS_PARAM_PREFIXES: &[&str] = &["gpt-5", "o1", "o3", "o4"];

pub struct OpenAiAdapter {
    base_url: String,
    extras: HttpOverrides,
    /// The server's profile, the provider's only copy; the request body writes a field only
    /// where the profile claims it. Many compatible servers (DeepSeek, Qwen, vLLM) take only
    /// `json_object` and reject `json_schema` with "This response_format type is unavailable now".
    profile: ProviderProfile,
    /// The server's dialect, [`StandardDialect`] unless set; the trait's default methods call this
    /// adapter's `*_default` ones.
    dialect: Arc<dyn OpenAiDialect>,
}

impl OpenAiAdapter {
    pub(super) fn new(base_url: String, extras: HttpOverrides, profile: ProviderProfile) -> Self {
        Self {
            base_url,
            extras,
            profile: limit_to_openai(profile),
            dialect: Arc::new(StandardDialect),
        }
    }

    pub(super) fn with_dialect(mut self, dialect: Arc<dyn OpenAiDialect>) -> Self {
        self.dialect = dialect;
        self
    }

    pub(super) fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    fn max_tokens_field(model: &str) -> &'static str {
        if NEW_TOKENS_PARAM_PREFIXES
            .iter()
            .any(|p| model.starts_with(p))
        {
            "max_completion_tokens"
        } else {
            "max_tokens"
        }
    }

    fn translate_message(m: &Message) -> Value {
        match m {
            Message::User { content } => json!({
                "role": "user",
                "content": Self::translate_content(content),
            }),
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut out = json!({
                    "role": "assistant",
                    "content": Self::translate_content(content),
                });
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            // OpenAI takes `arguments` as a JSON string. Serializing a `Value`
                            // fails only for a map with non-string keys, which a `Value` cannot
                            // hold.
                            let args_str = serde_json::to_string(&tc.arguments)
                                .expect("ToolCall.arguments must serialize");
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": wire_tool_name(&tc.name),
                                    "arguments": args_str,
                                }
                            })
                        })
                        .collect();
                    out["tool_calls"] = Value::Array(calls);
                }
                out
            }
            Message::Tool {
                tool_call_id,
                content,
                is_error,
            } => {
                // OpenAI's tool message has no error flag; without the marker a failed tool reads
                // as a success.
                let mut content_blocks = Self::translate_content(content);
                if *is_error && let Value::Array(arr) = &mut content_blocks {
                    arr.insert(
                        0,
                        json!({"type": "text", "text": "[tool execution failed]\n"}),
                    );
                }
                json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content_blocks,
                })
            }
            Message::System { content } => json!({
                "role": "system",
                "content": Self::translate_content(content),
            }),
        }
    }

    /// Returns the content as an array of blocks, the form that also carries images.
    fn translate_content(blocks: &[ContentBlock]) -> Value {
        let mut out = Vec::with_capacity(blocks.len());
        for b in blocks {
            match b {
                ContentBlock::Text { text } => {
                    out.push(json!({"type": "text", "text": text}));
                }
                ContentBlock::Image { mime, data } => {
                    let url = match data {
                        tars_types::ImageData::Url(u) => u.clone(),
                        tars_types::ImageData::Base64(b) => format!("data:{mime};base64,{b}"),
                    };
                    out.push(json!({
                        "type": "image_url",
                        "image_url": {"url": url},
                    }));
                }
            }
        }
        Value::Array(out)
    }

    fn translate_tools(tools: &[tars_types::ToolSpec]) -> Value {
        let arr: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": wire_tool_name(&t.name),
                        "description": t.description,
                        "parameters": t.input_schema.schema(),
                        "strict": t.input_schema.strict,
                    }
                })
            })
            .collect();
        Value::Array(arr)
    }

    /// Returns `{base_url}/files{suffix}` with the configured `query_params`: `""` to upload,
    /// `/{id}/content` to download one file.
    pub(crate) fn files_url(&self, suffix: &str) -> Result<Url, ProviderError> {
        self.url_with_query(&format!("files{suffix}"))
    }

    /// Returns `{base_url}/batches{suffix}` with the configured `query_params`: `""` for the
    /// collection, `/{id}` for one job, `/{id}/cancel` to cancel it.
    pub(crate) fn batches_url(&self, suffix: &str) -> Result<Url, ProviderError> {
        self.url_with_query(&format!("batches{suffix}"))
    }

    fn url_with_query(&self, path: &str) -> Result<Url, ProviderError> {
        let trimmed = self.base_url.trim_end_matches('/');
        let mut url = parse_url(format!("{trimmed}/{path}"))?;
        self.extras.apply_query_params(&mut url);
        Ok(url)
    }

    pub(crate) fn build_request_default(
        &self,
        req: &ChatRequest,
        model: &str,
    ) -> Result<Value, ProviderError> {
        // OpenAI rejects an empty user message with a 400.
        for m in &req.messages {
            if let Message::User { content } = m
                && content.is_empty()
            {
                return Err(ProviderError::InvalidRequest(
                    RequestRefusal::EmptyUserMessage,
                ));
            }
        }

        let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
        if let Some(sys) = &req.system {
            messages.push(json!({
                "role": "system",
                "content": [{"type": "text", "text": sys}],
            }));
        }
        for m in &req.messages {
            // `req.system` wins over inline system messages, which are dropped.
            if req.system.is_some() && matches!(m, Message::System { .. }) {
                continue;
            }
            messages.push(Self::translate_message(m));
        }

        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });

        if let Some(max) = req.max_output_tokens {
            let field = Self::max_tokens_field(model);
            body[field] = json!(max);
        }

        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if !req.stop_sequences.is_empty() {
            body["stop"] = json!(req.stop_sequences);
        }
        if let Some(seed) = req.seed {
            body["seed"] = json!(seed);
        }

        if !req.tools.is_empty() {
            check_wire_names_distinct(&req.tools)?;
            body["tools"] = Self::translate_tools(&req.tools);
            body["tool_choice"] = match &req.tool_choice {
                tars_types::ToolChoice::Auto => json!("auto"),
                tars_types::ToolChoice::None => json!("none"),
                tars_types::ToolChoice::Required => json!("required"),
                tars_types::ToolChoice::Specific(name) => json!({
                    "type": "function",
                    "function": {"name": wire_tool_name(name)},
                }),
            };
            // OpenAI does not hold parallel calls to a strict schema, so strict tools turn them
            // off.
            if req.tools.iter().any(|t| t.input_schema.strict) {
                body["parallel_tool_calls"] = json!(false);
            }
        }

        if let Some(schema) = &req.structured_output {
            match self.profile.supports_structured_output {
                // A server the profile has no data on is sent the schema and answers for itself.
                Some(StructuredOutputMode::StrictSchema) | None => {
                    body["response_format"] = json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": schema.name.clone().unwrap_or_else(|| "Response".to_string()),
                            "schema": crate::provider::schema_adapt::adapt_schema(
                                &schema.to_value(),
                                crate::provider::schema_adapt::SchemaDialect::OpenAi,
                            )
                            .map_err(|e| {
                                ProviderError::InvalidRequest(RequestRefusal::ResponseSchema(e))
                            })?
                            .schema,
                            "strict": schema.strict,
                        }
                    });
                }
                Some(StructuredOutputMode::JsonObjectMode) => {
                    // The schema is not enforced; the caller's prompt must give the shape and
                    // contain the word "json", which `json_object` requires.
                    body["response_format"] = json!({ "type": "json_object" });
                }
                Some(StructuredOutputMode::None | StructuredOutputMode::ToolUseEmulation) => {}
            }
        }

        // `None` leaves the chat template's default. A server without a chat template (OpenAI
        // rejects the field) is not sent the switch, and the dispatch check marks the response
        // degraded.
        if let (true, Some(enable)) = (
            self.profile.supports_chat_template_thinking,
            req.enable_chat_template_thinking,
        ) {
            body["chat_template_kwargs"] = json!({"enable_thinking": enable});
        }

        Ok(body)
    }

    pub(crate) fn parse_event_default(
        &self,
        raw: &SseEvent,
        buf: &mut StreamDecodeState,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        if raw.data.trim() == "[DONE]" {
            // Some servers and proxies send no `finish_reason`; the tool calls still end here.
            let mut emitted = buf.end_open_calls(StreamEnd::Terminator)?;
            // A server that ignores `include_usage` sends no usage chunk; `Finished` comes here,
            // without usage. With no stop reason seen either, no `Finished` is sent.
            emitted.extend(ChatEvent::stream_end(buf.take_pending_stop(), None));
            return Ok(emitted);
        }

        let v: Value = serde_json::from_str(&raw.data).map_err(|e| ProviderError::Parse {
            at: "openai sse",
            line: None,
            problem: Box::new(ParseProblem::Json(e)),
            raw: Some(truncate(&raw.data, 200)),
        })?;

        let mut out = Vec::new();

        // The usage chunk comes last, with empty `choices`.
        if let Some(usage) = v.get("usage").and_then(|u| u.as_object()) {
            let choices_empty = v
                .get("choices")
                .and_then(|c| c.as_array())
                .is_none_or(|a| a.is_empty());
            if choices_empty {
                // OpenAI sends `finish_reason` one chunk before the usage. A usage chunk with no
                // stop reason waiting sends no `Finished`, since the stream did not say why the
                // turn stopped; its usage goes out as `UsageProgress`. After a `Finished` that
                // carried usage, a usage chunk is not sent at all.
                if !buf.finished_emitted() {
                    let end = ChatEvent::stream_end(
                        buf.take_pending_stop(),
                        Some(self.dialect.parse_usage(usage)),
                    );
                    if end.as_ref().is_some_and(ChatEvent::is_terminal) {
                        buf.mark_finished();
                    }
                    out.extend(end);
                }
                return Ok(out);
            }
        }

        let choices = match v.get("choices").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => return Ok(out),
        };

        let model = v
            .get("model")
            .and_then(|m| m.as_str())
            .filter(|m| !m.is_empty());
        if let Some(model) = model
            && buf.take_started()
        {
            out.push(ChatEvent::started(model));
        }

        for choice in choices {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            let events = message_events(delta).map_err(|problem| ProviderError::Parse {
                at: "openai tool_call delta",
                line: None,
                problem: Box::new(problem),
                raw: Some(truncate(&raw.data, 200)),
            })?;
            track_tool_calls(buf, &events);
            out.extend(events);

            if let Some(reason_str) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                let stop = map_finish_reason(reason_str);
                out.extend(buf.end_inflight(stop)?);

                // Without usage in this chunk, `Finished` waits for the usage chunk.
                if let Some(usage_obj) = v.get("usage").and_then(|u| u.as_object()) {
                    out.push(ChatEvent::Finished {
                        stop_reason: stop,
                        usage: Some(self.dialect.parse_usage(usage_obj)),
                    });
                    buf.mark_finished();
                } else {
                    buf.record_pending_stop(stop);
                }
            }
        }

        Ok(out)
    }
}

#[async_trait]
impl HttpAdapter for OpenAiAdapter {
    fn build_url(&self, _model: &str) -> Result<Url, ProviderError> {
        let trimmed = self.base_url.trim_end_matches('/');
        parse_url(format!("{trimmed}/chat/completions"))
    }

    fn build_headers(&self, auth: &ResolvedAuth) -> Result<HeaderMap, ProviderError> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        match auth {
            ResolvedAuth::Bearer(t) | ResolvedAuth::ApiKey(t) => {
                let value = HeaderValue::from_str(&format!("Bearer {t}")).map_err(|source| {
                    ProviderError::Auth(CredentialError::Header {
                        header: "authorization",
                        source,
                    })
                })?;
                h.insert(AUTHORIZATION, value);
            }
            ResolvedAuth::None => {}
        }
        Ok(h)
    }

    fn translate_request(&self, req: &ChatRequest, model: &str) -> Result<Value, ProviderError> {
        self.dialect.build_request(self, req, model)
    }

    fn parse_event(
        &self,
        raw: &SseEvent,
        buf: &mut StreamDecodeState,
    ) -> Result<Vec<ChatEvent>, ProviderError> {
        self.dialect.parse_event(self, raw, buf)
    }

    fn classify_error(
        &self,
        status: StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> ProviderError {
        let message = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.get("message")).cloned())
            .and_then(|m| m.as_str().map(|s| s.to_string()))
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

/// Returns the error of an OpenAI error response from its status and message; `retry_after`
/// is its `Retry-After`.
fn response_error(status: u16, message: String, retry_after: Option<Duration>) -> ProviderError {
    let lower = message.to_lowercase();
    let says_overflow = lower.contains("context_length")
        || lower.contains("maximum context length")
        || lower.contains("too many tokens");
    match status {
        413 => ProviderError::ContextTooLong {
            limit: None,
            requested: None,
            message,
        },
        400 | 422 if says_overflow => ProviderError::ContextTooLong {
            limit: None,
            requested: None,
            message,
        },
        400 | 422 => ProviderError::InvalidRequest(RequestRefusal::Provider(ProviderRefusal {
            status: Some(status),
            message,
        })),
        _ => status_error(status, message, retry_after),
    }
}

/// Returns `name` in OpenAI's name pattern, with `.` written as `-`.
fn wire_tool_name(name: &str) -> String {
    name.replace('.', "-")
}

/// Returns the name of the tool in `tools` that travels as `wire`; a name no tool has is returned
/// as the model sent it.
pub(super) fn tool_name_from_wire(wire: &str, tools: &[tars_types::ToolSpec]) -> String {
    tools
        .iter()
        .find(|t| wire_tool_name(&t.name) == wire)
        .map_or_else(|| wire.to_string(), |t| t.name.clone())
}

/// Refuses two tools that travel under one name (`a.b` and `a-b`), since a call to it could not
/// be read back as either.
fn check_wire_names_distinct(tools: &[tars_types::ToolSpec]) -> Result<(), ProviderError> {
    let mut seen = std::collections::HashMap::new();
    for t in tools {
        if let Some(other) = seen.insert(wire_tool_name(&t.name), &t.name)
            && *other != t.name
        {
            return Err(ProviderError::InvalidRequest(
                RequestRefusal::WireNameCollision {
                    first: other.clone(),
                    second: t.name.clone(),
                    wire: wire_tool_name(&t.name),
                },
            ));
        }
    }
    Ok(())
}

/// Returns `profile` narrowed to what this adapter sends. Tool-use emulation is not built here
/// (only Anthropic's adapter forces a tool for a schema), so a profile that claims it says there
/// is no mechanism, and the dispatch check marks the response degraded instead of the schema
/// being dropped unseen.
fn limit_to_openai(mut profile: ProviderProfile) -> ProviderProfile {
    if profile.supports_structured_output == Some(StructuredOutputMode::ToolUseEmulation) {
        profile.supports_structured_output = Some(StructuredOutputMode::None);
    }
    profile
}

/// Returns a profile that claims only the structured output `mode` (`None`: no data on it), for
/// the tests' adapters.
#[cfg(test)]
pub(super) fn profile_with(mode: Option<StructuredOutputMode>) -> ProviderProfile {
    ProviderProfile {
        supports_structured_output: mode,
        ..ProviderProfile::nothing_claimed(tars_types::InterfaceKind::Http)
    }
}

#[cfg(test)]
mod tests {
    use super::super::provider::DEFAULT_BASE_URL;
    use super::*;

    #[test]
    fn picks_max_completion_tokens_for_o1() {
        assert_eq!(
            OpenAiAdapter::max_tokens_field("o1-preview"),
            "max_completion_tokens"
        );
        assert_eq!(
            OpenAiAdapter::max_tokens_field("gpt-5-something"),
            "max_completion_tokens"
        );
        assert_eq!(OpenAiAdapter::max_tokens_field("gpt-4o"), "max_tokens");
    }

    #[test]
    fn translates_simple_user_message() {
        let m = Message::user_text("hello");
        let v = OpenAiAdapter::translate_message(&m);
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hello");
    }

    #[test]
    fn tool_message_marks_failure_when_is_error_set() {
        let m = Message::Tool {
            tool_call_id: "tu_1".into(),
            content: vec![tars_types::ContentBlock::text("permission denied")],
            is_error: true,
        };
        let v = OpenAiAdapter::translate_message(&m);
        assert_eq!(v["role"], "tool");
        assert_eq!(v["content"][0]["text"], "[tool execution failed]\n");
        assert_eq!(v["content"][1]["text"], "permission denied");
    }

    #[test]
    fn tool_message_no_marker_on_success() {
        let m = Message::Tool {
            tool_call_id: "tu_1".into(),
            content: vec![tars_types::ContentBlock::text("42")],
            is_error: false,
        };
        let v = OpenAiAdapter::translate_message(&m);
        assert_eq!(v["content"][0]["text"], "42");
        assert!(v["content"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn structured_output_response_format_follows_provider_mode() {
        let schema = tars_types::JsonSchema::strict(
            "Findings",
            tars_types::json_object! {"type": "object", "properties": {"findings": {"type": "array"}}},
        );
        let req = |s: Option<tars_types::JsonSchema>| ChatRequest {
            system: None,
            messages: vec![Message::user_text("scan this; output json")],
            tools: vec![],
            tool_choice: Default::default(),
            structured_output: s,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            seed: None,
            cache_directives: vec![],
            thinking: None,
            enable_chat_template_thinking: None,
        };
        let mk = |mode| {
            OpenAiAdapter::new(
                DEFAULT_BASE_URL.into(),
                HttpOverrides::default(),
                profile_with(mode),
            )
        };

        let strict = mk(Some(StructuredOutputMode::StrictSchema))
            .translate_request(&req(Some(schema.clone())), "gpt-4o")
            .unwrap();
        assert_eq!(strict["response_format"]["type"], "json_schema");

        let jobj = mk(Some(StructuredOutputMode::JsonObjectMode))
            .translate_request(&req(Some(schema.clone())), "gpt-4o")
            .unwrap();
        assert_eq!(
            jobj["response_format"]["type"], "json_object",
            "openai_compat must get json_object, never the rejected json_schema",
        );

        let none = mk(Some(StructuredOutputMode::None))
            .translate_request(&req(Some(schema.clone())), "gpt-4o")
            .unwrap();
        assert!(
            none.get("response_format").is_none(),
            "mode=None emits no response_format",
        );

        let unknown = mk(None)
            .translate_request(&req(Some(schema.clone())), "gpt-4o")
            .unwrap();
        assert_eq!(
            unknown["response_format"]["type"], "json_schema",
            "a server with no data on structured output is sent the schema",
        );

        let bare = mk(Some(StructuredOutputMode::StrictSchema))
            .translate_request(&req(None), "gpt-4o")
            .unwrap();
        assert!(bare.get("response_format").is_none());
    }

    #[test]
    fn a_profile_claiming_tool_use_emulation_says_this_adapter_has_no_mechanism() {
        let built = |mode| {
            OpenAiAdapter::new(
                DEFAULT_BASE_URL.into(),
                HttpOverrides::default(),
                profile_with(mode),
            )
            .profile()
            .supports_structured_output
        };
        assert_eq!(
            built(Some(StructuredOutputMode::ToolUseEmulation)),
            Some(StructuredOutputMode::None)
        );
        for kept in [
            None,
            Some(StructuredOutputMode::None),
            Some(StructuredOutputMode::JsonObjectMode),
            Some(StructuredOutputMode::StrictSchema),
        ] {
            assert_eq!(built(kept), kept);
        }
    }

    #[test]
    fn shared_builder_never_emits_thinking_field() {
        use tars_types::ThinkingMode;
        let req = |t: ThinkingMode| ChatRequest {
            system: None,
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            tool_choice: Default::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            seed: None,
            cache_directives: vec![],
            thinking: Some(t),
            enable_chat_template_thinking: None,
        };
        for (base, mode) in [
            (
                "https://api.deepseek.com",
                StructuredOutputMode::JsonObjectMode,
            ),
            (DEFAULT_BASE_URL, StructuredOutputMode::StrictSchema),
        ] {
            let body = OpenAiAdapter::new(
                base.into(),
                HttpOverrides::default(),
                profile_with(Some(mode)),
            )
            .translate_request(&req(ThinkingMode::Auto), "gpt-4o")
            .unwrap();
            assert!(
                body.get("thinking").is_none(),
                "shared builder must not emit `thinking` (base_url={base})",
            );
        }
    }

    #[test]
    fn translate_request_dedups_system_when_req_system_set() {
        let a = OpenAiAdapter::new(
            DEFAULT_BASE_URL.into(),
            HttpOverrides::default(),
            profile_with(Some(StructuredOutputMode::StrictSchema)),
        );
        let req = ChatRequest {
            system: Some("explicit system".into()),
            messages: vec![
                Message::System {
                    content: vec![tars_types::ContentBlock::text("inline system")],
                },
                Message::user_text("hi"),
            ],
            tools: vec![],
            tool_choice: Default::default(),
            structured_output: None,
            max_output_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            seed: None,
            cache_directives: vec![],
            thinking: None,
            enable_chat_template_thinking: None,
        };
        let body = a.translate_request(&req, "gpt-4o").unwrap();
        let messages = body["messages"].as_array().unwrap();
        let system_count = messages.iter().filter(|m| m["role"] == "system").count();
        assert_eq!(system_count, 1, "should dedupe to one system message");
        assert_eq!(messages[0]["content"][0]["text"], "explicit system");
    }

    fn empty_headers() -> reqwest::header::HeaderMap {
        reqwest::header::HeaderMap::new()
    }

    #[test]
    fn classify_401_is_auth() {
        let a = OpenAiAdapter::new(
            DEFAULT_BASE_URL.into(),
            HttpOverrides::default(),
            profile_with(Some(StructuredOutputMode::StrictSchema)),
        );
        let err = a.classify_error(
            StatusCode::UNAUTHORIZED,
            &empty_headers(),
            "{\"error\":{\"message\":\"bad\"}}",
        );
        assert!(matches!(err, ProviderError::Auth(_)));
    }

    #[test]
    fn classify_429_is_rate_limited() {
        let a = OpenAiAdapter::new(
            DEFAULT_BASE_URL.into(),
            HttpOverrides::default(),
            profile_with(Some(StructuredOutputMode::StrictSchema)),
        );
        let err = a.classify_error(StatusCode::TOO_MANY_REQUESTS, &empty_headers(), "");
        assert!(matches!(
            err,
            ProviderError::RateLimited { retry_after: None }
        ));
    }

    #[test]
    fn classify_429_with_retry_after_seconds_populates_field() {
        let a = OpenAiAdapter::new(
            DEFAULT_BASE_URL.into(),
            HttpOverrides::default(),
            profile_with(Some(StructuredOutputMode::StrictSchema)),
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "42".parse().unwrap());
        let err = a.classify_error(StatusCode::TOO_MANY_REQUESTS, &headers, "");
        match err {
            ProviderError::RateLimited { retry_after } => {
                assert_eq!(retry_after, Some(std::time::Duration::from_secs(42)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_400_context_length_is_typed() {
        let a = OpenAiAdapter::new(
            DEFAULT_BASE_URL.into(),
            HttpOverrides::default(),
            profile_with(Some(StructuredOutputMode::StrictSchema)),
        );
        let body = r#"{"error":{"message":"context_length_exceeded: too many tokens"}}"#;
        let err = a.classify_error(StatusCode::BAD_REQUEST, &empty_headers(), body);
        assert!(matches!(err, ProviderError::ContextTooLong { .. }));
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::provider::backends::openai::StandardDialect;
    use tars_types::{JsonSchema, ToolSpec};

    fn tool(name: &str, strict: bool) -> ToolSpec {
        let schema = if strict {
            JsonSchema::strict("A", tars_types::json_object! {"type": "object"})
        } else {
            JsonSchema::loose(tars_types::json_object! {"type": "object"})
        };
        ToolSpec::new(name, "d", schema).unwrap()
    }

    fn reads_template(mut a: OpenAiAdapter) -> OpenAiAdapter {
        a.profile.supports_chat_template_thinking = true;
        a
    }

    fn adapter(dialect: Arc<dyn OpenAiDialect>) -> OpenAiAdapter {
        OpenAiAdapter::new(
            "http://localhost/v1".into(),
            HttpOverrides::default(),
            profile_with(Some(StructuredOutputMode::StrictSchema)),
        )
        .with_dialect(dialect)
    }

    #[test]
    fn a_tool_name_with_a_dash_or_a_dot_comes_back_as_the_request_had_it() {
        let tools = [tool("web-search", false), tool("fs.read", false)];
        assert_eq!(tool_name_from_wire("web-search", &tools), "web-search");
        assert_eq!(tool_name_from_wire("fs-read", &tools), "fs.read");
        assert_eq!(tool_name_from_wire("made-up", &tools), "made-up");
    }

    #[test]
    fn two_tools_that_travel_under_one_name_are_refused() {
        let mut req = ChatRequest::user("hi");
        req.tools = vec![tool("a.b", false), tool("a-b", false)];
        let err = adapter(Arc::new(StandardDialect))
            .build_request_default(&req, "gpt-4o")
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)), "{err:?}");
        req.tools = vec![tool("a.b", false), tool("a_b", false)];
        assert!(
            adapter(Arc::new(StandardDialect))
                .build_request_default(&req, "gpt-4o")
                .is_ok()
        );
    }

    #[test]
    fn strict_tools_turn_parallel_calls_off_and_loose_ones_leave_the_default() {
        let mut req = ChatRequest::user("hi");
        req.tools = vec![tool("f", true)];
        let body = adapter(Arc::new(StandardDialect))
            .build_request_default(&req, "gpt-4o")
            .unwrap();
        assert_eq!(body["parallel_tool_calls"], json!(false));
        req.tools = vec![tool("f", false)];
        let body = adapter(Arc::new(StandardDialect))
            .build_request_default(&req, "gpt-4o")
            .unwrap();
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn chat_template_kwargs_go_only_to_a_server_whose_profile_reads_them() {
        let mut req = ChatRequest::user("hi");
        req.enable_chat_template_thinking = Some(false);
        let body = reads_template(adapter(Arc::new(StandardDialect)))
            .build_request_default(&req, "qwen3")
            .unwrap();
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            json!(false)
        );
        let body = adapter(Arc::new(StandardDialect))
            .build_request_default(&req, "gpt-4o")
            .unwrap();
        assert!(body.get("chat_template_kwargs").is_none());
        req.enable_chat_template_thinking = None;
        let body = reads_template(adapter(Arc::new(StandardDialect)))
            .build_request_default(&req, "qwen3")
            .unwrap();
        assert!(body.get("chat_template_kwargs").is_none());
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
            adapter(Arc::new(StandardDialect))
                .classify_error(status, &headers, "{}")
                .class()
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
