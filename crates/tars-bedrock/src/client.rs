//! The keyless, lazily-signed Bedrock client and the aggregate
//! `complete_response` entry point (Doc 31 §6 C3 / §8.1).
//!
//! **Keyless** — there is no `Auth`/api-key here. The AWS SDK's default
//! credential chain (env / profile / SSO / ECS / EC2 IMDS) resolves the
//! signing identity and SigV4-signs every request; tars handles no key
//! material (Doc 31 §5/§13).
//!
//! **Lazy client** — `ProviderRegistry::build_one` is synchronous but the
//! AWS client build (`aws_config::defaults(..).load().await`) is async, so
//! the client is constructed once, on first use, behind a
//! [`tokio::sync::OnceCell`]. Credential-chain I/O is deferred to the
//! first call, and a misconfigured identity surfaces as a classified
//! [`ProviderError`] on that call rather than at registry-build time.
//!
//! This crate deliberately does **not** depend on `tars-provider`: it owns
//! only the AWS-specific mapping + transport and returns canonical
//! `tars-types` values. The thin `impl LlmProvider` that adapts this
//! client to the provider trait lives in `tars-provider` (behind its
//! `bedrock` feature), so the crate graph stays acyclic — the leaf crate
//! cannot depend on the crate that owns the trait *and* be depended on by
//! it.

use std::collections::HashSet;
use std::pin::Pin;

use aws_config::BehaviorVersion;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::config::Region;
use futures::Stream;
use tokio::sync::OnceCell;

use tars_types::{
    Capabilities, ChatEvent, ChatRequest, ChatResponse, Modality, PromptCacheKind, ProviderError,
    StructuredOutputMode,
};

use crate::error::{classify_sdk_error, classify_stream_event_error, classify_stream_send_error};
use crate::mapping::{build_converse, converse_output_to_response};
use crate::stream::StreamTranslator;

/// The incremental streaming return type: a `'static + Send` stream of
/// canonical events. `'static` because the stream owns everything it needs
/// (the SDK `EventReceiver` + translator), borrowing nothing from `self` —
/// so the bridge in `tars-provider` can hand it straight back as an
/// `LlmEventStream`.
pub type BedrockEventStream =
    Pin<Box<dyn Stream<Item = Result<ChatEvent, ProviderError>> + Send + 'static>>;

/// A keyless Bedrock Converse client for one `region` + `model`
/// (+ optional local `profile`). Cheap to hold; the underlying signed AWS
/// client is built once on first use.
pub struct BedrockClient {
    region: String,
    model: String,
    profile: Option<String>,
    /// Lazily built, once, on first use (§8.1). `OnceCell` never caches a
    /// failed init, so a transient credential-endpoint blip on the first
    /// call can be retried on the next.
    client: OnceCell<Client>,
}

impl BedrockClient {
    pub fn new(region: String, model: String, profile: Option<String>) -> Self {
        Self {
            region,
            model,
            profile,
            client: OnceCell::new(),
        }
    }

    /// The configured model id (e.g. `us.anthropic.claude-sonnet-4-5-v1:0`).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Resolve (once) the SigV4-signing client from the AWS credential
    /// chain. Building the `SdkConfig` + `Client` is itself infallible —
    /// credential *resolution* is deferred by the SDK to the first signed
    /// request, so a bad identity surfaces from `converse().send()`, not
    /// here.
    async fn client(&self) -> &Client {
        self.client
            .get_or_init(|| async {
                let mut loader = aws_config::defaults(BehaviorVersion::latest())
                    .region(Region::new(self.region.clone()));
                if let Some(p) = &self.profile {
                    loader = loader.profile_name(p.clone());
                }
                let cfg = loader.load().await;
                Client::new(&cfg)
            })
            .await
    }

    /// Non-streaming completion (Doc 31 §8.2): map → sign → unary
    /// `converse()` → classify → replay into a [`ChatResponse`]. Unary
    /// `converse()` is strictly cheaper than `converse_stream()` for the
    /// aggregate case, so both the trait `complete` and the M0 `stream`
    /// (converse-then-replay) go through here.
    pub async fn complete_response(
        &self,
        req: &ChatRequest,
        model: &str,
    ) -> Result<ChatResponse, ProviderError> {
        let parts = build_converse(req)?;
        let client = self.client().await;
        let out = client
            .converse()
            .model_id(model.to_string())
            .set_system(parts.system)
            .set_messages(Some(parts.messages))
            .set_tool_config(parts.tool_config)
            .set_inference_config(parts.inference)
            .send()
            .await
            .map_err(classify_sdk_error)?;
        converse_output_to_response(&out, model)
    }

    /// Streaming completion (Doc 31 §6 C2 / M1): map → sign →
    /// `converse_stream()` → translate each `ConverseStreamOutput` event
    /// into a canonical [`ChatEvent`] *incrementally*.
    ///
    /// The request-open error (`send`) is classified eagerly so a bad
    /// model / auth failure surfaces from this `await` rather than as the
    /// stream's first item; per-event errors (`recv`) are classified
    /// inside the stream and terminate it (all carrying the SDK's own
    /// message — CLAUDE.md #1). The transport owns the leading `Started`
    /// event; the trailing `Finished` comes from [`StreamTranslator::finish`].
    pub async fn stream_response(
        &self,
        req: &ChatRequest,
        model: &str,
    ) -> Result<BedrockEventStream, ProviderError> {
        let parts = build_converse(req)?;
        let client = self.client().await;
        let out = client
            .converse_stream()
            .model_id(model.to_string())
            .set_system(parts.system)
            .set_messages(Some(parts.messages))
            .set_tool_config(parts.tool_config)
            .set_inference_config(parts.inference)
            .send()
            .await
            .map_err(classify_stream_send_error)?;

        // Move the owned event receiver into the generator so the stream
        // borrows nothing from `self` (stays `'static`).
        let mut receiver = out.stream;
        let model = model.to_string();
        let events = async_stream::try_stream! {
            yield ChatEvent::started(model);
            let mut translator = StreamTranslator::new();
            while let Some(event) = receiver.recv().await.map_err(classify_stream_event_error)? {
                for ev in translator.translate(&event)? {
                    yield ev;
                }
            }
            for ev in translator.finish() {
                yield ev;
            }
        };
        Ok(Box::pin(events))
    }
}

/// Conservative default capability descriptor. Bedrock hosts many model
/// families; these defaults suit the common Claude/Nova chat case and can
/// be overridden per-provider by the caller.
pub fn default_capabilities() -> Capabilities {
    let mut modalities_in = HashSet::new();
    modalities_in.insert(Modality::Text);
    modalities_in.insert(Modality::Image);
    Capabilities {
        max_context_tokens: 200_000,
        max_output_tokens: 8_192,
        supports_tool_use: true,
        supports_parallel_tool_calls: true,
        supports_structured_output: StructuredOutputMode::ToolUseEmulation,
        supports_vision: true,
        // M0 does not yet translate the reasoning knob into the
        // model-family `additionalModelRequestFields` (see mapping.rs).
        supports_thinking: false,
        supports_cancel: true,
        prompt_cache: PromptCacheKind::ExplicitMarker,
        streaming: true,
        modalities_in,
        modalities_out: HashSet::from([Modality::Text]),
        pricing: tars_types::Pricing::default(),
    }
}
