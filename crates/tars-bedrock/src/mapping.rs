//! Pure, no-I/O conversion between tars canonical types and the AWS SDK's
//! typed Converse structs (Doc 31 §6 C1). Mirrors the role of
//! `tars-provider/src/backends/anthropic/mapping.rs` — the layer that can
//! be tested without any transport.
//!
//! One mapping covers **all** Bedrock models: Converse is Bedrock's
//! cross-model normalization shape, so we never write per-model bodies.

use aws_sdk_bedrockruntime::operation::converse::ConverseOutput as AwsConverseOutput;
use aws_sdk_bedrockruntime::types::{
    AnyToolChoice, AutoToolChoice, ContentBlock as AwsContentBlock, ConversationRole,
    ConverseOutput as ConverseOutputBody, ImageBlock, ImageFormat, ImageSource,
    InferenceConfiguration, Message as AwsMessage, ReasoningContentBlock,
    SpecificToolChoice, StopReason as AwsStopReason, SystemContentBlock, TokenUsage, Tool,
    ToolChoice as AwsToolChoice, ToolConfiguration, ToolInputSchema, ToolResultBlock,
    ToolResultContentBlock, ToolResultStatus, ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::Blob;

use tars_types::{
    ChatEvent, ChatRequest, ChatResponse, ChatResponseBuilder, ContentBlock, ImageData, Message,
    ProviderError, StopReason, ToolChoice, Usage,
};

use crate::document::{document_to_value, value_to_document};

/// The four inputs the Bedrock `converse()` / `converse_stream()` fluent
/// builders take, produced from a canonical [`ChatRequest`]. Passed to the
/// SDK via `set_system` / `set_messages` / `set_tool_config` /
/// `set_inference_config` (Doc 31 §8.2).
#[derive(Debug, Clone)]
pub struct ConverseParts {
    pub system: Option<Vec<SystemContentBlock>>,
    pub messages: Vec<AwsMessage>,
    pub tool_config: Option<ToolConfiguration>,
    pub inference: Option<InferenceConfiguration>,
}

/// `ChatRequest` → the Converse request pieces. Total: every canonical
/// field that Converse can express is mapped; a field Converse *cannot*
/// express (an image by URL — Converse only accepts inline bytes or S3)
/// is a hard [`ProviderError::InvalidRequest`] carrying the real reason,
/// never a silent drop (CLAUDE.md #1/#3).
pub fn build_converse(req: &ChatRequest) -> Result<ConverseParts, ProviderError> {
    // ── system blocks ──────────────────────────────────────────────
    // `req.system` plus any inline `Message::System` text (some callers
    // keep system turns in the history rather than hoisting them).
    let mut system_blocks: Vec<SystemContentBlock> = Vec::new();
    if let Some(sys) = &req.system {
        if !sys.is_empty() {
            system_blocks.push(SystemContentBlock::Text(sys.clone()));
        }
    }

    // ── messages ───────────────────────────────────────────────────
    let mut messages: Vec<AwsMessage> = Vec::new();
    for m in &req.messages {
        match m {
            Message::System { content } => {
                for cb in content {
                    if let Some(t) = cb.as_text() {
                        system_blocks.push(SystemContentBlock::Text(t.to_string()));
                    }
                }
            }
            Message::User { content } => {
                let blocks = map_content_blocks(content)?;
                messages.push(build_message(ConversationRole::User, blocks)?);
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks = map_content_blocks(content)?;
                // Assistant tool-call replay → ToolUse content blocks so a
                // multi-turn history round-trips (the model sees its own
                // prior calls). `arguments` is already parsed JSON.
                for tc in tool_calls {
                    let tu = ToolUseBlock::builder()
                        .tool_use_id(tc.id.clone())
                        .name(tc.name.clone())
                        .input(value_to_document(&tc.arguments))
                        .build()
                        .map_err(|e| {
                            ProviderError::InvalidRequest(format!("bedrock tool_use block: {e}"))
                        })?;
                    blocks.push(AwsContentBlock::ToolUse(tu));
                }
                messages.push(build_message(ConversationRole::Assistant, blocks)?);
            }
            Message::Tool {
                tool_call_id,
                content,
                is_error,
            } => {
                // Converse carries tool results inside a *user*-role message
                // with a ToolResult content block referencing the call id.
                let result_content: Vec<ToolResultContentBlock> = content
                    .iter()
                    .filter_map(|cb| cb.as_text().map(|t| ToolResultContentBlock::Text(t.to_string())))
                    .collect();
                let mut trb = ToolResultBlock::builder()
                    .tool_use_id(tool_call_id.clone())
                    .set_content(Some(result_content));
                if *is_error {
                    trb = trb.status(ToolResultStatus::Error);
                }
                let trb = trb.build().map_err(|e| {
                    ProviderError::InvalidRequest(format!("bedrock tool_result block: {e}"))
                })?;
                messages.push(build_message(
                    ConversationRole::User,
                    vec![AwsContentBlock::ToolResult(trb)],
                )?);
            }
        }
    }

    // ── tool config ────────────────────────────────────────────────
    let tool_config = if req.tools.is_empty() {
        None
    } else {
        let mut tools: Vec<Tool> = Vec::with_capacity(req.tools.len());
        for spec in &req.tools {
            if !spec.has_valid_name() {
                return Err(ProviderError::InvalidRequest(
                    "bedrock tool spec: name must be non-empty".into(),
                ));
            }
            let schema = ToolInputSchema::Json(value_to_document(&spec.input_schema.schema));
            let ts = ToolSpecification::builder()
                .name(spec.name.clone())
                .description(spec.description.clone())
                .input_schema(schema)
                .build()
                .map_err(|e| {
                    ProviderError::InvalidRequest(format!("bedrock tool spec {}: {e}", spec.name))
                })?;
            tools.push(Tool::ToolSpec(ts));
        }
        let choice = map_tool_choice(&req.tool_choice)?;
        let cfg = ToolConfiguration::builder()
            .set_tools(Some(tools))
            .set_tool_choice(choice)
            .build()
            .map_err(|e| ProviderError::InvalidRequest(format!("bedrock tool_config: {e}")))?;
        Some(cfg)
    };

    // ── inference config ───────────────────────────────────────────
    let inference = if req.max_output_tokens.is_none()
        && req.temperature.is_none()
        && req.stop_sequences.is_empty()
    {
        None
    } else {
        let stop = if req.stop_sequences.is_empty() {
            None
        } else {
            Some(req.stop_sequences.clone())
        };
        Some(
            InferenceConfiguration::builder()
                // `u32 → i32`: Bedrock's field is i32; a caller value above
                // i32::MAX is nonsensical for a token cap — clamp rather
                // than wrap, so we never send a negative token count.
                .set_max_tokens(req.max_output_tokens.map(|n| i32::try_from(n).unwrap_or(i32::MAX)))
                .set_temperature(req.temperature)
                .set_stop_sequences(stop)
                .build(),
        )
    };

    // NOTE (M1): `req.thinking` is not yet translated. The reasoning knob
    // is model-family-specific (`additionalModelRequestFields`), and
    // forcing it on a model that doesn't support it 400s. Wiring it per
    // model family is deferred to M1 with ConverseStream (Doc 31 §6 C1).

    Ok(ConverseParts {
        system: if system_blocks.is_empty() {
            None
        } else {
            Some(system_blocks)
        },
        messages,
        tool_config,
        inference,
    })
}

fn build_message(
    role: ConversationRole,
    content: Vec<AwsContentBlock>,
) -> Result<AwsMessage, ProviderError> {
    AwsMessage::builder()
        .role(role)
        .set_content(Some(content))
        .build()
        .map_err(|e| ProviderError::InvalidRequest(format!("bedrock message: {e}")))
}

fn map_content_blocks(blocks: &[ContentBlock]) -> Result<Vec<AwsContentBlock>, ProviderError> {
    blocks.iter().map(map_content_block).collect()
}

fn map_content_block(cb: &ContentBlock) -> Result<AwsContentBlock, ProviderError> {
    match cb {
        ContentBlock::Text { text } => Ok(AwsContentBlock::Text(text.clone())),
        ContentBlock::Image { mime, data } => {
            let format = image_format(mime);
            let source = match data {
                ImageData::Base64(b64) => {
                    // Converse wants raw bytes; our canonical form is base64.
                    let bytes = aws_smithy_types::base64::decode(b64).map_err(|e| {
                        ProviderError::InvalidRequest(format!("bedrock image: bad base64: {e}"))
                    })?;
                    ImageSource::Bytes(Blob::new(bytes))
                }
                ImageData::Url(url) => {
                    // Converse's ImageSource is Bytes | S3Location only — an
                    // arbitrary URL cannot be represented. Tell the truth
                    // rather than drop the image.
                    return Err(ProviderError::InvalidRequest(format!(
                        "bedrock Converse cannot send an image by URL ({url}); \
                         inline the bytes (base64) or use an S3 location"
                    )));
                }
            };
            let img = ImageBlock::builder()
                .format(format)
                .source(source)
                .build()
                .map_err(|e| ProviderError::InvalidRequest(format!("bedrock image block: {e}")))?;
            Ok(AwsContentBlock::Image(img))
        }
    }
}

/// `image/png` → [`ImageFormat::Png`]. Bedrock's `ImageFormat` `From<&str>`
/// takes the bare subtype (`png`, `jpeg`, …); strip the `image/` prefix.
fn image_format(mime: &str) -> ImageFormat {
    let subtype = mime.strip_prefix("image/").unwrap_or(mime);
    ImageFormat::from(subtype)
}

/// Canonical [`ToolChoice`] → Bedrock [`AwsToolChoice`]. `None` maps to
/// `Ok(None)`: Converse has no "forbid all tools" choice, so we omit the
/// field (model default) — the tools themselves are still declared.
fn map_tool_choice(tc: &ToolChoice) -> Result<Option<AwsToolChoice>, ProviderError> {
    Ok(match tc {
        ToolChoice::Auto => Some(AwsToolChoice::Auto(AutoToolChoice::builder().build())),
        ToolChoice::Required => Some(AwsToolChoice::Any(AnyToolChoice::builder().build())),
        ToolChoice::Specific(name) => {
            let sc = SpecificToolChoice::builder()
                .name(name.clone())
                .build()
                .map_err(|e| {
                    ProviderError::InvalidRequest(format!("bedrock tool_choice {name}: {e}"))
                })?;
            Some(AwsToolChoice::Tool(sc))
        }
        ToolChoice::None => None,
    })
}

/// Bedrock `StopReason` → canonical [`StopReason`]. Kept in lockstep with
/// the anthropic mapping's unknown-reason fallback (`Other`).
pub fn map_stop_reason(s: &AwsStopReason) -> StopReason {
    match s {
        AwsStopReason::EndTurn => StopReason::EndTurn,
        AwsStopReason::MaxTokens => StopReason::MaxTokens,
        AwsStopReason::StopSequence => StopReason::StopSequence,
        AwsStopReason::ToolUse => StopReason::ToolUse,
        AwsStopReason::ContentFiltered | AwsStopReason::GuardrailIntervened => {
            StopReason::ContentFilter
        }
        // ModelContextWindowExceeded / MalformedModelOutput / MalformedToolUse
        // / any future variant → Other (check provider logs).
        _ => StopReason::Other,
    }
}

/// Bedrock `TokenUsage` → canonical [`Usage`]. Bedrock's `input_tokens`
/// is the *total* prompt (OpenAI-style, like our contract), so cache reads
/// are a subset of it — no re-addition needed (contrast Anthropic).
pub fn parse_usage(u: &TokenUsage) -> Usage {
    // Bedrock reports i32; a token count is never negative. Clamp a
    // (spec-impossible) negative to 0 rather than wrap into a huge u64.
    let nn = |n: i32| u64::try_from(n).unwrap_or(0);
    Usage {
        input_tokens: nn(u.input_tokens),
        output_tokens: nn(u.output_tokens),
        cached_input_tokens: u.cache_read_input_tokens.map(nn).unwrap_or(0),
        cache_creation_tokens: u.cache_write_input_tokens.map(nn).unwrap_or(0),
        thinking_tokens: 0,
    }
}

/// Non-streaming: an `AwsConverseOutput` → [`ChatResponse`], replayed
/// through [`ChatResponseBuilder`] exactly as
/// `anthropic/mapping.rs:message_to_chat_response` does. `model_id` names
/// the resolved model for the `Started` event (the Converse output body
/// carries no model id of its own).
pub fn converse_output_to_response(
    out: &AwsConverseOutput,
    model_id: &str,
) -> Result<ChatResponse, ProviderError> {
    let mut acc = ChatResponseBuilder::new();
    acc.apply(ChatEvent::started(model_id));

    if let Some(ConverseOutputBody::Message(msg)) = out.output() {
        let mut tool_index = 0usize;
        for block in msg.content() {
            match block {
                AwsContentBlock::Text(t) => acc.apply(ChatEvent::Delta { text: t.clone() }),
                AwsContentBlock::ReasoningContent(rc) => {
                    if let Some(text) = reasoning_text(rc) {
                        acc.apply(ChatEvent::ThinkingDelta { text });
                    }
                }
                AwsContentBlock::ToolUse(tu) => {
                    acc.apply(ChatEvent::ToolCallStart {
                        index: tool_index,
                        id: tu.tool_use_id().to_string(),
                        name: tu.name().to_string(),
                    });
                    acc.apply(ChatEvent::ToolCallEnd {
                        index: tool_index,
                        id: tu.tool_use_id().to_string(),
                        parsed_args: document_to_value(tu.input()),
                        thought_signature: None,
                    });
                    tool_index += 1;
                }
                // Image / Document / other assistant-emitted blocks aren't
                // part of the canonical text/thinking/tool response surface.
                _ => {}
            }
        }
    }

    let stop_reason = map_stop_reason(out.stop_reason());
    let usage = out.usage().map(parse_usage).unwrap_or_default();
    acc.apply(ChatEvent::Finished { stop_reason, usage });
    Ok(acc.finish())
}

fn reasoning_text(rc: &ReasoningContentBlock) -> Option<String> {
    match rc {
        ReasoningContentBlock::ReasoningText(t) => Some(t.text.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tars_types::{JsonSchema, ModelHint, ToolSpec};

    #[test]
    fn build_converse_shapes_system_messages_and_tools() {
        // E2E-2: system + user turn + a tool with a JSON schema + a
        // Specific tool_choice must produce the right Converse shape,
        // with the tool schema carried as a Document (no silent drop).
        let mut req = ChatRequest::user("hello");
        req.system = Some("be terse".into());
        req.tools.push(
            ToolSpec::new(
                "search",
                "search the web",
                JsonSchema::loose(json!({
                    "type": "object",
                    "properties": { "q": { "type": "string" } },
                    "required": ["q"]
                })),
            )
            .unwrap(),
        );
        req.tool_choice = ToolChoice::Specific("search".into());
        req.max_output_tokens = Some(256);
        req.temperature = Some(0.2);

        let parts = build_converse(&req).unwrap();

        // system
        let system = parts.system.expect("system block present");
        assert_eq!(system.len(), 1);
        assert!(matches!(&system[0], SystemContentBlock::Text(t) if t == "be terse"));

        // messages: one user turn with a text block
        assert_eq!(parts.messages.len(), 1);
        assert_eq!(*parts.messages[0].role(), ConversationRole::User);
        assert!(matches!(
            &parts.messages[0].content()[0],
            AwsContentBlock::Text(t) if t == "hello"
        ));

        // tool config: one ToolSpec whose input schema round-trips as JSON
        let tc = parts.tool_config.expect("tool config present");
        assert_eq!(tc.tools().len(), 1);
        let Tool::ToolSpec(ts) = &tc.tools()[0] else {
            panic!("expected ToolSpec variant");
        };
        assert_eq!(ts.name(), "search");
        let ToolInputSchema::Json(doc) = ts.input_schema().unwrap() else {
            panic!("expected Json schema");
        };
        // The schema Document must round-trip back to the original JSON.
        assert_eq!(
            document_to_value(doc),
            json!({
                "type": "object",
                "properties": { "q": { "type": "string" } },
                "required": ["q"]
            })
        );
        // Specific tool_choice → SpecificToolChoice(name)
        assert!(matches!(
            tc.tool_choice(),
            Some(AwsToolChoice::Tool(sc)) if sc.name() == "search"
        ));

        // inference config carried max_tokens + temperature
        let inf = parts.inference.expect("inference config present");
        assert_eq!(inf.max_tokens(), Some(256));
        assert_eq!(inf.temperature(), Some(0.2));
    }

    #[test]
    fn build_converse_url_image_is_honest_error() {
        use tars_types::ContentBlock;
        let mut req = ChatRequest::user("look");
        if let Message::User { content } = &mut req.messages[0] {
            content.push(ContentBlock::Image {
                mime: "image/png".into(),
                data: ImageData::Url("https://x/y.png".into()),
            });
        }
        let err = build_converse(&req).unwrap_err();
        match err {
            ProviderError::InvalidRequest(m) => {
                assert!(m.contains("URL"), "must name the real reason, got: {m}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn parse_usage_treats_input_as_total() {
        let u = TokenUsage::builder()
            .input_tokens(100)
            .output_tokens(40)
            .total_tokens(140)
            .cache_read_input_tokens(30)
            .build()
            .unwrap();
        let usage = parse_usage(&u);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 40);
        assert_eq!(usage.cached_input_tokens, 30);
    }

    #[test]
    fn stop_reason_maps_known_and_unknown() {
        assert_eq!(map_stop_reason(&AwsStopReason::EndTurn), StopReason::EndTurn);
        assert_eq!(map_stop_reason(&AwsStopReason::ToolUse), StopReason::ToolUse);
        assert_eq!(
            map_stop_reason(&AwsStopReason::ModelContextWindowExceeded),
            StopReason::Other
        );
    }

    #[test]
    fn converse_output_to_response_extracts_text_and_usage() {
        // E2E-1: a ConverseOutput fixture (text + usage + stop reason) →
        // ChatResponse. No AWS call.
        let msg = AwsMessage::builder()
            .role(ConversationRole::Assistant)
            .content(AwsContentBlock::Text("hi there".into()))
            .build()
            .unwrap();
        let usage = TokenUsage::builder()
            .input_tokens(12)
            .output_tokens(3)
            .total_tokens(15)
            .build()
            .unwrap();
        let out = AwsConverseOutput::builder()
            .output(ConverseOutputBody::Message(msg))
            .stop_reason(AwsStopReason::EndTurn)
            .usage(usage)
            .build()
            .unwrap();

        let resp = converse_output_to_response(&out, "us.anthropic.claude").unwrap();
        assert_eq!(resp.text, "hi there");
        assert_eq!(resp.actual_model, "us.anthropic.claude");
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 3);
        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn converse_output_to_response_extracts_tool_use() {
        let tu = ToolUseBlock::builder()
            .tool_use_id("call_1")
            .name("search")
            .input(value_to_document(&json!({ "q": "rust" })))
            .build()
            .unwrap();
        let msg = AwsMessage::builder()
            .role(ConversationRole::Assistant)
            .content(AwsContentBlock::ToolUse(tu))
            .build()
            .unwrap();
        let out = AwsConverseOutput::builder()
            .output(ConverseOutputBody::Message(msg))
            .stop_reason(AwsStopReason::ToolUse)
            .build()
            .unwrap();

        let resp = converse_output_to_response(&out, "m").unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "search");
        assert_eq!(resp.tool_calls[0].arguments, json!({ "q": "rust" }));
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
    }
}
