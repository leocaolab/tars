//! Node.js bindings for tars: a [`Pipeline`] or [`Provider`] handle whose async `complete()` makes
//! one non-streaming chat call.
//!
//! ```ts
//! import { Pipeline } from '@leocaolab/tars-node';
//!
//! const pipeline = Pipeline.fromConfigPath('.arc/config.toml', 'gemini_pro');
//! const resp = await pipeline.complete({
//!     model: 'gemini-3.1-pro-preview',
//!     user: 'Hello',
//!     system: 'You are a critic.',
//!     responseSchema: { type: 'object', /* … */ },
//!     responseSchemaStrict: true,
//!     maxOutputTokens: 3000,
//!     temperature: 0.0,
//! });
//!
//! console.log(resp.text);
//! console.log(resp.usage.inputTokens, resp.usage.outputTokens);
//! ```
//!
//! The async functions run on napi's own tokio runtime. Streaming, tool calls, cancellation and
//! the DAG executor are not exposed.

#![deny(clippy::print_stdout, clippy::print_stderr)]

mod ctx;
mod errors;
mod handle;

// Re-exported so the rlib test build sees the free `#[napi]` functions as used; napi registers
// them at their definitions, not here.
pub use ctx::JsContext;
pub use handle::{
    Provider, init, is_initialized, pipeline, provider, role_model, role_provider, tars_home,
};

use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::ctx::build_context;
use crate::errors::{REGISTRY_ERROR, config_to_js, llm_reason, registry_to_js};

use tars_config::Config;
use tars_llm::{ChainOptions, LlmService, RetryConfig};
use tars_llm::{LlmProvider, ProviderRegistry};
use tars_types::{
    Caller, ChatRequest, ContentBlock, JsonSchema, Message, ParseProviderIdError, ProviderId,
    RUN_CONTEXT, ThinkingMode,
};

/// Returns a greeting; the Node smoke test calls it to check that the addon loads.
#[napi]
pub fn hello(name: String) -> String {
    format!("tars-node says hi, {name}")
}

/// Kills every subprocess tars started in this process that is still running, with the processes
/// they started in turn. The addon installs no signal handler, since a listener would replace
/// Node's default exit on SIGINT and SIGTERM. The host calls this from its own signal handlers;
/// a `process.on('exit')` listener covers a normal exit, but a default signal exit skips it.
#[napi]
pub fn kill_spawned_children() {
    tars_llm::kill_all_spawned();
}

// ── TS-facing shapes ─────────────────────────────────────────────────

/// The options of one `complete()` call: tars-py's `Pipeline.complete(**kwargs)` in camelCase.
#[napi(object)]
pub struct CompleteOptions {
    pub model: String,
    /// The user turn; exactly one of this and `messages` is given.
    pub user: Option<String>,
    pub system: Option<String>,
    /// An OpenAI-style `[{ role, content }]` list, so a caller that already has one passes it
    /// as is.
    pub messages: Option<serde_json::Value>,
    /// Unset leaves the provider's default.
    pub max_output_tokens: Option<u32>,
    /// Unset leaves the provider's default.
    pub temperature: Option<f64>,
    /// `true` lets the provider choose the thinking depth, `false` turns thinking off; absent
    /// leaves the provider's default.
    pub thinking: Option<bool>,
    /// A JSON Schema the reply must conform to; its root must be an object.
    pub response_schema: Option<serde_json::Value>,
    /// Whether the provider enforces `response_schema` (default) or takes it as a hint.
    pub response_schema_strict: Option<bool>,
}

/// What `complete()` resolves to.
#[napi(object)]
pub struct CompleteResult {
    pub text: String,
    /// The tokens the call used; absent when the provider reported none.
    pub usage: Option<UsageJs>,
    /// The model the provider reports having used, which routing can make differ from the one
    /// asked for; absent when the provider did not say.
    pub model: Option<String>,
    /// One of `end_turn`, `max_tokens`, `stop_sequence`, `tool_use`, `content_filter`,
    /// `cancelled`, `other`.
    pub stop_reason: String,
    /// The parts of the request the provider served it without (`structured_output`,
    /// `chat_template_thinking`); empty when it served all of it.
    pub degraded: Vec<String>,
}

/// The token counts of one call: `tars_types::Usage`, each count cast to `u32` for JS.
#[napi(object)]
pub struct UsageJs {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: u32,
    pub cache_creation_tokens: u32,
    pub thinking_tokens: u32,
}

/// Builds the chain for the model each `complete({ model })` names, for factory- and role-built
/// pipelines alike. The model is part of every cassette key, so any other model (the
/// provider's default, the role's) would key the call to a model the caller did not ask for.
struct ProviderHandle {
    provider: Arc<dyn LlmProvider>,
    retry: RetryConfig,
}

impl ProviderHandle {
    fn for_model(&self, model: &str) -> LlmService {
        let mut opts = ChainOptions::default();
        opts.retry = Some(self.retry.clone());
        LlmService::default_chain(Arc::clone(&self.provider), model, opts)
    }
}

/// One provider behind the default middleware chain. The factories read and parse the config and
/// build the provider, so a process builds one per provider at startup and keeps it.
#[napi]
pub struct Pipeline {
    id: String,
    service: ProviderHandle,
    /// The caller's context, taken from JS by every constructor and kept for the pipeline's life.
    /// No call runs under it: each `complete()` runs under its own copy from
    /// [`Caller::for_call`].
    ctx: Caller,
}

#[napi]
impl Pipeline {
    /// Builds a pipeline over `providerId` from the config file at `path`.
    ///
    /// # Errors
    ///
    /// Rejects with code `InvalidArg` when `ctx` is missing or holds an empty id or when
    /// `providerId` is empty, with `TarsConfigError` when the file does not load, and with
    /// `TarsRegistryError` when the provider is not in it or any provider in it cannot be built.
    #[napi(
        factory,
        ts_args_type = "path: string, providerId: string, ctx: JsContext"
    )]
    pub fn from_config_path(
        path: String,
        provider_id: String,
        ctx: Option<JsContext>,
    ) -> Result<Pipeline, String> {
        let ctx = factory_context(ctx)?;
        let cfg = Config::load_file(&path).map_err(config_to_js)?;
        let provider = build_provider_from_cfg(&cfg, &provider_id)?;
        Ok(Self::from_provider(
            provider_id,
            provider,
            RetryConfig::default(),
            ctx,
        ))
    }

    /// Builds a pipeline from inline TOML shaped like a config file; it fails as
    /// [`Self::from_config_path`] does.
    #[napi(
        factory,
        ts_args_type = "tomlText: string, providerId: string, ctx: JsContext"
    )]
    pub fn from_str(
        toml_text: String,
        provider_id: String,
        ctx: Option<JsContext>,
    ) -> Result<Pipeline, String> {
        let ctx = factory_context(ctx)?;
        let cfg = Config::from_toml(&toml_text).map_err(config_to_js)?;
        let provider = build_provider_from_cfg(&cfg, &provider_id)?;
        Ok(Self::from_provider(
            provider_id,
            provider,
            RetryConfig::default(),
            ctx,
        ))
    }

    /// Returns the id of the provider this pipeline was built over.
    #[napi(getter)]
    pub fn id(&self) -> String {
        self.id.clone()
    }

    /// Makes one non-streaming chat call through the default chain for `opts.model`.
    #[napi]
    pub async fn complete(&self, opts: CompleteOptions) -> Result<CompleteResult> {
        let service = self.service.for_model(&opts.model);
        drive_complete(service, &self.ctx, opts).await
    }
}

impl Pipeline {
    /// Puts `provider` behind the default chain (retry, telemetry and the rest), retrying per
    /// `retry`.
    pub(crate) fn from_provider(
        id: String,
        provider: Arc<dyn LlmProvider>,
        retry: RetryConfig,
        ctx: Caller,
    ) -> Self {
        let service = ProviderHandle { provider, retry };
        Self { id, service, ctx }
    }
}

/// Refuses a factory's ctx with its message and the code `InvalidArg`.
fn factory_context(js: Option<JsContext>) -> Result<Caller, String> {
    build_context(js).map_err(|e| Error::new(INVALID_ARG.to_string(), e.reason))
}

/// The `.code` a factory refuses an argument with, napi's own word for it.
const INVALID_ARG: &str = "InvalidArg";

/// Runs one non-streaming completion through `svc` under a new call of `caller`, which is also
/// `RUN_CONTEXT` for the call's duration so code reading the task-local sees the same
/// accumulators.
pub(crate) async fn drive_complete(
    svc: LlmService,
    caller: &Caller,
    opts: CompleteOptions,
) -> Result<CompleteResult> {
    let ctx = caller.for_call();
    let req = build_request(opts)?;
    RUN_CONTEXT
        .scope(ctx.clone(), async move {
            let resp = tars_llm::complete_async(svc, req, ctx)
                .await
                .map_err(llm_reason)?;
            Ok(CompleteResult {
                text: resp.text,
                usage: resp.usage.map(|usage| UsageJs {
                    input_tokens: usage.input_tokens as u32,
                    output_tokens: usage.output_tokens as u32,
                    cached_input_tokens: usage.cached_input_tokens as u32,
                    cache_creation_tokens: usage.cache_creation_tokens as u32,
                    thinking_tokens: usage.thinking_tokens as u32,
                }),
                model: resp.actual_model,
                stop_reason: stop_reason_str(&resp.stop_reason).to_string(),
                degraded: resp.degraded.iter().map(|d| d.kind().to_string()).collect(),
            })
        })
        .await
}

// ── Builder helpers ──────────────────────────────────────────────────

/// Builds the registry from `cfg` and takes `provider_id` out of it; a missing id lists the
/// configured ones.
fn build_provider_from_cfg(
    cfg: &tars_config::Config,
    provider_id: &str,
) -> Result<Arc<dyn LlmProvider>, String> {
    let pid: ProviderId = provider_id.parse().map_err(|e: ParseProviderIdError| {
        Error::new(INVALID_ARG.to_string(), format!("providerId: {e}"))
    })?;
    let registry =
        ProviderRegistry::from_config_default(cfg.providers(), tars_config::catalog(), None)
            .map_err(registry_to_js)?;
    let provider = registry.get(&pid).map_err(|e| match e {
        tars_llm::RegistryError::UnknownProvider(_) => {
            let configured: Vec<String> = cfg
                .providers()
                .iter()
                .map(|(id, _)| id.to_string())
                .collect();
            Error::new(
                REGISTRY_ERROR.to_string(),
                format!(
                    "provider {provider_id:?} not in config. Configured: [{}]",
                    configured.join(", "),
                ),
            )
        }
        other => registry_to_js(other),
    })?;
    Ok(provider)
}

fn stop_reason_str(r: &tars_types::StopReason) -> &'static str {
    use tars_types::StopReason::*;
    match r {
        EndTurn => "end_turn",
        MaxTokens => "max_tokens",
        StopSequence => "stop_sequence",
        ToolUse => "tool_use",
        ContentFilter => "content_filter",
        Cancelled => "cancelled",
        Other => "other",
    }
}

fn build_request(opts: CompleteOptions) -> Result<ChatRequest> {
    if opts.user.is_some() && opts.messages.is_some() {
        return Err(Error::from_reason(
            "pass either `user` (single-turn) or `messages` (multi-turn), not both",
        ));
    }
    // The request is model-agnostic content; `opts.model` binds the service
    // (`ProviderHandle::for_model`), not the request.
    let mut req = ChatRequest {
        system: opts.system,
        messages: Vec::new(),
        tools: Vec::new(),
        tool_choice: Default::default(),
        structured_output: None,
        max_output_tokens: opts.max_output_tokens,
        temperature: opts.temperature.map(|t| t as f32),
        stop_sequences: Vec::new(),
        seed: None,
        cache_directives: Vec::new(),
        thinking: opts.thinking.map(map_thinking),
        enable_chat_template_thinking: opts.thinking,
    };
    if let Some(user) = opts.user {
        req.messages.push(Message::user_text(user));
    } else if let Some(msgs) = opts.messages {
        req.messages = parse_messages_json(msgs)?;
    } else {
        return Err(Error::from_reason(
            "must pass at least one of `user` or `messages`",
        ));
    }
    if let Some(schema) = opts.response_schema {
        let mut schema = JsonSchema::loose(
            tars_types::schema_root(schema)
                .map_err(|e| Error::from_reason(format!("`responseSchema`: {e}")))?,
        );
        schema.strict = opts.response_schema_strict.unwrap_or(true);
        req.structured_output = Some(schema);
    }
    Ok(req)
}

fn map_thinking(enabled: bool) -> ThinkingMode {
    if enabled {
        ThinkingMode::Auto
    } else {
        ThinkingMode::Off
    }
}

/// Parses an OpenAI-style `[{ role, content }]` list whose roles are `user`, `assistant` or
/// `system` and whose contents are strings; anything else is refused naming the entry.
fn parse_messages_json(v: serde_json::Value) -> Result<Vec<Message>> {
    let arr = v
        .as_array()
        .ok_or_else(|| Error::from_reason("`messages` must be a JSON array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, m) in arr.iter().enumerate() {
        let role = m
            .get("role")
            .and_then(|r| r.as_str())
            .ok_or_else(|| Error::from_reason(format!("messages[{i}].role missing")))?;
        let content = m.get("content").and_then(|c| c.as_str()).ok_or_else(|| {
            Error::from_reason(format!("messages[{i}].content missing or not a string"))
        })?;
        let msg = match role {
            "user" => Message::user_text(content.to_string()),
            "assistant" => Message::assistant_text(content.to_string()),
            "system" => Message::System {
                content: vec![ContentBlock::text(content.to_string())],
            },
            other => {
                return Err(Error::from_reason(format!(
                    "messages[{i}].role unknown: {other:?} (expected user|assistant|system)"
                )));
            }
        };
        out.push(msg);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOCK_CONFIG: &str = r#"
[providers.demo]
type = "mock"
canned_response = "ok"
"#;

    #[test]
    fn a_factory_built_pipeline_is_bound_to_the_callers_tenant() {
        let ctx = JsContext {
            tenant: "acme".to_string(),
            session: None,
            trace: None,
        };
        let pipeline =
            Pipeline::from_str(MOCK_CONFIG.to_string(), "demo".to_string(), Some(ctx)).unwrap();
        assert_eq!(pipeline.ctx.tenant_id, tars_types::TenantId::new("acme"));
    }

    /// Starts a reply and cuts the stream before it finishes.
    struct CutProvider {
        id: ProviderId,
        profile: tars_types::ProviderProfile,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CutProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn capabilities(&self) -> &tars_types::ProviderProfile {
            &self.profile
        }

        async fn stream(
            self: Arc<Self>,
            _req: ChatRequest,
            _model: &str,
            _ctx: Caller,
        ) -> std::result::Result<tars_llm::LlmEventStream, tars_llm::ProviderError> {
            let events = vec![
                Ok(tars_types::ChatEvent::started("m")),
                Err(tars_llm::ProviderError::StreamCut { open_call: None }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn a_stream_cut_mid_reply_reaches_js_under_its_kind() {
        let provider = Arc::new(CutProvider {
            id: ProviderId::new("cut"),
            profile: tars_types::ProviderProfile::text_only_baseline(Default::default()),
        });
        let svc = LlmService::default_chain(provider, "m", ChainOptions::default());
        let caller = Caller::new(
            tars_types::TenantId::new("acme"),
            tars_types::TraceId::new("t"),
        );
        let opts = CompleteOptions {
            response_schema: None,
            ..options_with_schema(serde_json::Value::Null)
        };

        let Err(err) = drive_complete(svc, &caller, opts).await else {
            panic!("a cut stream completed");
        };
        assert!(err.reason.starts_with("stream_cut: "), "{}", err.reason);
    }

    #[test]
    fn an_empty_provider_id_is_an_invalid_argument_not_a_panic() {
        let ctx = JsContext {
            tenant: "acme".to_string(),
            session: None,
            trace: None,
        };
        let Err(err) = Pipeline::from_str(MOCK_CONFIG.to_string(), String::new(), Some(ctx)) else {
            panic!("an empty provider id built a pipeline");
        };
        assert_eq!(err.status, INVALID_ARG);
        assert!(err.reason.starts_with("providerId: "), "{}", err.reason);
    }

    fn options_with_schema(schema: serde_json::Value) -> CompleteOptions {
        CompleteOptions {
            model: "m".to_string(),
            user: Some("hi".to_string()),
            system: None,
            messages: None,
            max_output_tokens: None,
            temperature: None,
            thinking: None,
            response_schema: Some(schema),
            response_schema_strict: None,
        }
    }

    #[test]
    fn a_response_schema_whose_root_is_not_an_object_is_refused() {
        let err = build_request(options_with_schema(serde_json::json!(["x"]))).unwrap_err();
        assert!(err.reason.contains("responseSchema"), "{}", err.reason);
    }

    #[test]
    fn a_response_schema_whose_root_is_an_object_is_sent() {
        let req =
            build_request(options_with_schema(serde_json::json!({"type": "object"}))).unwrap();
        assert!(req.structured_output.is_some());
    }
}
