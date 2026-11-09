//! Drives a request through an [`LlmService`]'s layers and streams from its provider at the end.
//!
//! The provider's own facts (`id`, `capabilities`, `count_tokens`, `cost`) stay on the provider;
//! the service only binds it to a model and a fixed stack of layers.

use std::sync::{Arc, Mutex, PoisonError};

use crate::{LlmEventStream, LlmProvider, ProviderError};
use futures::{StreamExt, stream};
use tars_types::{
    CallId, CallTelemetry, Caller, ChatEvent, ChatRequest, CompatibilityCheck, Degradation,
    OutputLimit, ProviderId,
};

use crate::LlmError;
use crate::pipeline::middleware::Middleware;

/// One provider bound to one model under a fixed stack of layers. The model is bound here, so a
/// [`ChatRequest`] never carries one.
#[derive(Clone)]
pub struct LlmService {
    provider: Arc<dyn LlmProvider>,
    model: String,
    /// The bound model's output ceiling, asked of the provider once at binding. A request with no
    /// `max_output_tokens` is sent with it; otherwise the wire protocol's own default applies,
    /// which on several providers is far below the model's ceiling.
    output_limit: OutputLimit,
    /// Outermost first.
    layers: Vec<Arc<dyn Middleware>>,
    /// Derived from `layers` at construction, so [`Self::layer_names`] can return a borrowed slice.
    layer_names: Arc<[&'static str]>,
}

impl LlmService {
    /// Binds `model` to `provider` with no layers.
    pub fn of(provider: Arc<dyn LlmProvider>, model: impl Into<String>) -> LlmService {
        LlmService::compose(provider, model, Vec::new())
    }

    /// Makes one streaming call with the bound model.
    ///
    /// The request is first checked against the provider's profile, once, before any layer sees
    /// it. One the provider cannot serve is refused: no layer handles it, and each is told of the
    /// refusal through [`Middleware::on_refusal`] before this returns, so the call log still gets
    /// its row. When the provider serves it without some parts, the stream starts with a
    /// [`ChatEvent::Degraded`] naming them and the request goes ahead. The event is added outside
    /// the layers; the layers read the same parts from [`Next::unsent`].
    ///
    /// A request with no `max_output_tokens` then gets the bound model's
    /// [`OutputLimit`], so every layer — the event
    /// record, the budget estimate — sees the value that goes
    /// on the wire. An explicit value is left as the caller set it.
    ///
    /// The returned [`ServiceCall`] names the `llm_call` rows this call was logged under,
    /// whichever way it ended. Its `result` is [`LlmError::Incompatible`] when the provider's
    /// profile cannot serve the request, and otherwise what the layers or the provider return.
    pub async fn call(&self, req: ChatRequest, ctx: Caller) -> ServiceCall {
        let ledger = CallLedger::default();
        // A failed open's measurements were the layers' to record; the caller gets the error.
        let result = self
            .run(&ledger, req, ctx)
            .await
            .map_err(|failure| failure.error);
        ServiceCall {
            call_ids: ledger.into_ids(),
            result,
        }
    }

    async fn run(
        &self,
        ledger: &CallLedger,
        mut req: ChatRequest,
        ctx: Caller,
    ) -> Result<LlmEventStream, OpenFailure> {
        // The caller's own `max_output_tokens` is checked, not the bound model's limit filled in
        // below: a provider-wide profile ceiling can sit below one model's own.
        let unsent = match req.compatibility_check(self.provider.capabilities()) {
            CompatibilityCheck::Compatible => Vec::new(),
            CompatibilityCheck::Degraded { unsent } => unsent,
            CompatibilityCheck::Incompatible { reasons } => {
                let error = LlmError::Incompatible { reasons };
                self.tell_refused(ledger, &req, &ctx, &error);
                return Err(error.into());
            }
        };
        req.max_output_tokens = max_output_tokens_sent(req.max_output_tokens, &self.output_limit);
        let next = Next {
            layers: &self.layers,
            provider: &self.provider,
            model: &self.model,
            output_limit: &self.output_limit,
            unsent: &unsent,
            ledger,
        };
        let events = next.run(req, ctx).await?;
        Ok(if unsent.is_empty() {
            events
        } else {
            Box::pin(stream::once(async { Ok(ChatEvent::Degraded { unsent }) }).chain(events))
        })
    }

    /// Tells every layer, outermost first, that this call is refused with `error`.
    fn tell_refused(&self, ledger: &CallLedger, req: &ChatRequest, ctx: &Caller, error: &LlmError) {
        let refusal = RefusedCall {
            request: req,
            ctx,
            model: &self.model,
            output_limit: &self.output_limit,
            error,
            ledger,
        };
        for layer in &self.layers {
            layer.on_refusal(&refusal);
        }
    }

    /// Returns the layer names, outermost first.
    pub fn layer_names(&self) -> &[&'static str] {
        &self.layer_names
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the bound model's output ceiling and where it came from.
    pub fn output_limit(&self) -> &OutputLimit {
        &self.output_limit
    }

    pub fn provider(&self) -> &Arc<dyn LlmProvider> {
        &self.provider
    }

    pub(crate) fn into_parts(self) -> (Arc<dyn LlmProvider>, String, Vec<Arc<dyn Middleware>>) {
        (self.provider, self.model, self.layers)
    }

    pub(crate) fn compose(
        provider: Arc<dyn LlmProvider>,
        model: impl Into<String>,
        layers: Vec<Arc<dyn Middleware>>,
    ) -> LlmService {
        let layer_names: Arc<[&'static str]> = layers.iter().map(|l| l.name()).collect();
        let model = model.into();
        let output_limit = provider.output_limit(&model);
        LlmService {
            provider,
            model,
            output_limit,
            layers,
            layer_names,
        }
    }
}

/// What one [`LlmService::call`] did: the `llm_call` rows it was logged under, and its result.
///
/// The ids are there whichever way the call ended, a refusal and a failure before any stream
/// included. There is one per event emitter the call passed through each time it ran: none
/// when no emitter is on the chain or it could not log the call, several when a layer outside
/// the emitter ran the rest of the chain more than once. They are in the order the emitters
/// logged them, and complete once `call` returns: no layer can keep a [`Next`] past it.
#[must_use = "the result of the call is in `result`"]
pub struct ServiceCall {
    pub call_ids: Vec<CallId>,
    pub result: Result<LlmEventStream, LlmError>,
}

/// A run of the rest of a chain that failed before its stream opened: the error, and what the
/// layers inside measured before it (the retries they started, the provider time of the attempts
/// that failed to open). On an opened stream the same measurements travel as
/// [`ChatEvent::Measured`] events instead. The measurements are boxed so the `Err` of every
/// layer's result stays as small as the error alone.
#[derive(Debug)]
pub struct OpenFailure {
    pub error: LlmError,
    pub measured: Box<CallTelemetry>,
}

impl OpenFailure {
    /// Returns the failure of `error` with what the layers inside measured before it.
    pub fn new(error: LlmError, measured: CallTelemetry) -> Self {
        Self {
            error,
            measured: Box::new(measured),
        }
    }
}

impl From<LlmError> for OpenFailure {
    fn from(error: LlmError) -> Self {
        Self::new(error, CallTelemetry::default())
    }
}

impl From<ProviderError> for OpenFailure {
    fn from(error: ProviderError) -> Self {
        LlmError::from(error).into()
    }
}

/// The ids one service call was logged under, in the order they were recorded. Only this
/// crate's event emitter records into it, through [`Next`] or [`RefusedCall`].
#[derive(Default)]
struct CallLedger(Mutex<Vec<CallId>>);

impl CallLedger {
    fn record(&self, id: CallId) {
        // A poisoned lock is recovered: a push cannot leave the list half-written, and the
        // poison only says another holder panicked.
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(id);
    }

    fn into_ids(self) -> Vec<CallId> {
        self.0.into_inner().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A call the dispatch layer refused because its provider cannot serve the request. Only the
/// dispatch layer makes one.
pub struct RefusedCall<'a> {
    request: &'a ChatRequest,
    ctx: &'a Caller,
    model: &'a str,
    output_limit: &'a OutputLimit,
    error: &'a LlmError,
    ledger: &'a CallLedger,
}

impl<'a> RefusedCall<'a> {
    /// Returns the request as the caller sent it: an unset `max_output_tokens` was not filled in,
    /// since nothing was sent.
    pub fn request(&self) -> &'a ChatRequest {
        self.request
    }

    pub fn ctx(&self) -> &'a Caller {
        self.ctx
    }

    /// Returns the model the refusing [`LlmService`] is bound to.
    pub fn model(&self) -> &'a str {
        self.model
    }

    pub fn output_limit(&self) -> &'a OutputLimit {
        self.output_limit
    }

    /// Returns the error the caller gets, a [`LlmError::Incompatible`] naming every way the
    /// provider cannot serve the request.
    pub fn error(&self) -> &'a LlmError {
        self.error
    }

    /// Records that the refused call was logged under `id`.
    pub(crate) fn record_call(&self, id: CallId) {
        self.ledger.record(id);
    }
}

/// Returns the `max_output_tokens` a request is sent with: the caller's own value when it set
/// one, else the bound model's limit. A test that looks a request up in a cassette before the
/// call applies it too, since the cassette fingerprints the request as sent.
pub fn max_output_tokens_sent(asked: Option<u32>, limit: &OutputLimit) -> Option<u32> {
    asked.or(limit.tokens())
}

/// Points at the rest of an [`LlmService`]'s chain. It is `Copy`, so a layer may run the rest
/// several times (retry) or not at all (a refusal).
#[derive(Clone, Copy)]
pub struct Next<'a> {
    layers: &'a [Arc<dyn Middleware>],
    provider: &'a Arc<dyn LlmProvider>,
    model: &'a str,
    output_limit: &'a OutputLimit,
    unsent: &'a [Degradation],
    ledger: &'a CallLedger,
}

impl<'a> Next<'a> {
    /// Returns the model the enclosing [`LlmService`] is bound to. The `&'a str` outlives the
    /// borrow of `self`, so a layer can hold it across `next.run(...)`.
    pub fn model(&self) -> &'a str {
        self.model
    }

    /// Returns the bound model's output ceiling, which an unset `max_output_tokens` was filled
    /// with.
    pub fn output_limit(&self) -> &'a OutputLimit {
        self.output_limit
    }

    /// Returns who serves the API in OpenTelemetry GenAI's words, from the provider's profile;
    /// `None` when tars does not know.
    pub fn gen_ai_provider_name(&self) -> Option<&'a str> {
        self.provider.capabilities().gen_ai_provider_name.as_deref()
    }

    /// Returns what the provider serves this request without; empty when it is sent whole.
    pub fn unsent(&self) -> &'a [Degradation] {
        self.unsent
    }

    /// Returns the id of the provider at the end of the chain.
    pub fn provider_id(&self) -> &'a ProviderId {
        self.provider.id()
    }

    /// Returns the names of the layers left in the chain, outermost first, then `provider`.
    pub fn layer_names(&self) -> impl Iterator<Item = &'static str> + 'a {
        self.layers
            .iter()
            .map(|layer| layer.name())
            .chain(std::iter::once(PROVIDER_LAYER))
    }

    /// Records that the call running through here is logged under `id`.
    pub(crate) fn record_call(&self, id: CallId) {
        self.ledger.record(id);
    }

    /// Runs the next layer, or streams from the provider when no layer is left.
    pub async fn run(self, req: ChatRequest, ctx: Caller) -> Result<LlmEventStream, OpenFailure> {
        match self.layers.split_first() {
            Some((head, tail)) => {
                let next = Next {
                    layers: tail,
                    provider: self.provider,
                    model: self.model,
                    output_limit: self.output_limit,
                    unsent: self.unsent,
                    ledger: self.ledger,
                };
                head.handle(req, ctx, next).await
            }
            None => stream_from_provider(self.provider, req, self.model, ctx).await,
        }
    }
}

/// The name the end of the chain goes by among the layer names.
const PROVIDER_LAYER: &str = "provider";

/// Streams from the provider and measures this attempt's provider time: it goes out as a
/// [`ChatEvent::Measured`] before the stream's terminal item, or with the error the attempt
/// failed to open with, so under retry the parts add up to every attempt's time.
async fn stream_from_provider(
    provider: &Arc<dyn LlmProvider>,
    req: ChatRequest,
    model: &str,
    ctx: Caller,
) -> Result<LlmEventStream, OpenFailure> {
    let started = std::time::Instant::now();
    let inner = match provider.clone().stream(req, model, ctx).await {
        Ok(inner) => inner,
        Err(error) => {
            return Err(OpenFailure::new(error.into(), provider_time(started)));
        }
    };

    let observed = async_stream::stream! {
        use futures::StreamExt;
        let mut s = inner;
        let mut finished = false;
        let mut failed = false;
        let mut measured = false;
        while let Some(ev) = s.next().await {
            match &ev {
                Ok(ChatEvent::Finished { .. }) => finished = true,
                Ok(_) => {}
                Err(_) => failed = true,
            }
            if (finished || failed) && !measured {
                measured = true;
                yield Ok(ChatEvent::Measured { telemetry: provider_time(started) });
            }
            yield ev;
        }
        if let Some(e) = unfinished(finished, failed) {
            yield Ok(ChatEvent::Measured { telemetry: provider_time(started) });
            yield Err(e);
        }
    };
    Ok(Box::pin(observed))
}

/// Returns the provider time of an attempt that started at `started`.
fn provider_time(started: std::time::Instant) -> CallTelemetry {
    CallTelemetry {
        provider_latency_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
        ..CallTelemetry::default()
    }
}

/// Returns the error a provider stream ends in when it ended with neither `Finished` nor an
/// error: every backend's stream passes here, so a reply that never said why the turn stopped
/// reaches no caller as a success. A stream that failed keeps its own error.
fn unfinished(finished: bool, failed: bool) -> Option<ProviderError> {
    (!finished && !failed).then_some(ProviderError::Unfinished)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderError;
    use crate::provider::backends::mock::{CannedResponse, MockProvider};
    use tars_types::ProviderId;

    #[test]
    fn an_unset_max_output_tokens_is_sent_as_the_models_limit() {
        let limit = OutputLimit::Configured { tokens: 4096 };
        assert_eq!(max_output_tokens_sent(None, &limit), Some(4096));
        assert_eq!(max_output_tokens_sent(None, &OutputLimit::Unknown), None);
    }

    #[test]
    fn a_set_max_output_tokens_is_sent_as_the_caller_set_it() {
        let limit = OutputLimit::Configured { tokens: 4096 };
        assert_eq!(max_output_tokens_sent(Some(99), &limit), Some(99));
    }

    /// Reports an output ceiling and keeps the `max_output_tokens` it was sent.
    struct Capped {
        inner: Arc<MockProvider>,
        seen: std::sync::Mutex<Option<Option<u32>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for Capped {
        fn id(&self) -> &tars_types::ProviderId {
            self.inner.id()
        }
        fn capabilities(&self) -> &tars_types::ProviderProfile {
            self.inner.capabilities()
        }
        fn output_limit(&self, model: &str) -> OutputLimit {
            assert_eq!(model, "m-1");
            OutputLimit::Configured { tokens: 12_345 }
        }
        async fn stream(
            self: Arc<Self>,
            req: ChatRequest,
            model: &str,
            ctx: Caller,
        ) -> Result<LlmEventStream, ProviderError> {
            *self.seen.lock().unwrap() = Some(req.max_output_tokens);
            self.inner.clone().stream(req, model, ctx).await
        }
    }

    #[tokio::test]
    async fn unset_max_output_is_filled_with_the_bound_limit_explicit_is_kept() {
        let capped = Arc::new(Capped {
            inner: MockProvider::new(ProviderId::new("capped"), CannedResponse::text("ok")),
            seen: std::sync::Mutex::new(None),
        });
        let svc = LlmService::of(capped.clone(), "m-1");
        assert_eq!(
            svc.output_limit(),
            &OutputLimit::Configured { tokens: 12_345 }
        );

        let s = svc
            .call(ChatRequest::user("ping"), Caller::test_default())
            .await
            .result
            .unwrap();
        drop(s);
        assert_eq!(*capped.seen.lock().unwrap(), Some(Some(12_345)));

        let mut req = ChatRequest::user("ping");
        req.max_output_tokens = Some(99);
        drop(svc.call(req, Caller::test_default()).await.result.unwrap());
        assert_eq!(*capped.seen.lock().unwrap(), Some(Some(99)));
    }

    /// Serves canned text under a given profile.
    struct Profiled {
        inner: Arc<MockProvider>,
        profile: tars_types::ProviderProfile,
    }

    #[async_trait::async_trait]
    impl LlmProvider for Profiled {
        fn id(&self) -> &tars_types::ProviderId {
            self.inner.id()
        }
        fn capabilities(&self) -> &tars_types::ProviderProfile {
            &self.profile
        }
        async fn stream(
            self: Arc<Self>,
            req: ChatRequest,
            model: &str,
            ctx: Caller,
        ) -> Result<LlmEventStream, ProviderError> {
            self.inner.clone().stream(req, model, ctx).await
        }
    }

    fn profiled(profile: tars_types::ProviderProfile) -> LlmService {
        let inner = MockProvider::new(ProviderId::new("profiled"), CannedResponse::text("ok"));
        LlmService::of(Arc::new(Profiled { inner, profile }), "m")
    }

    async fn response_of(svc: &LlmService, req: ChatRequest) -> tars_types::ChatResponse {
        let mut events = svc.call(req, Caller::test_default()).await.result.unwrap();
        let mut b = tars_types::ChatResponseBuilder::new();
        while let Some(ev) = events.next().await {
            b.apply(ev.unwrap());
        }
        b.finish()
    }

    #[tokio::test]
    async fn a_switch_the_provider_cannot_take_is_sent_without_it_and_named_on_the_response() {
        let svc = profiled(tars_types::ProviderProfile::nothing_claimed(
            tars_types::InterfaceKind::Http,
        ));
        let mut req = ChatRequest::user("ping");
        req.enable_chat_template_thinking = Some(false);
        let degraded = response_of(&svc, req).await;
        assert_eq!(
            degraded.degraded,
            [Degradation::ChatTemplateThinkingNotSent]
        );
        assert!(degraded.is_finished());

        let whole = response_of(&svc, ChatRequest::user("ping")).await;
        assert!(whole.degraded.is_empty());
    }

    #[tokio::test]
    async fn a_request_for_what_the_provider_has_no_data_on_is_sent() {
        let svc = profiled(tars_types::ProviderProfile::undescribed_http());
        let mut req = ChatRequest::user("ping");
        req.thinking = tars_types::ThinkingMode::Auto;
        let resp = response_of(&svc, req).await;
        assert!(resp.is_finished());
        assert!(resp.degraded.is_empty());
    }

    #[tokio::test]
    async fn a_request_the_provider_cannot_serve_is_refused_before_any_layer() {
        let svc = profiled(tars_types::ProviderProfile::nothing_claimed(
            tars_types::InterfaceKind::Http,
        ));
        let mut req = ChatRequest::user("ping");
        req.thinking = tars_types::ThinkingMode::Auto;
        let refused = svc
            .run(&CallLedger::default(), req, Caller::test_default())
            .await;
        let Err(refused) = refused else {
            panic!("a request the provider cannot serve opens no stream");
        };
        assert!(matches!(refused.error, LlmError::Incompatible { .. }));
        assert!(refused.measured.provider_latency_ms.is_none());
    }

    /// Counts the calls it handles and the refusals it is told of.
    #[derive(Default)]
    struct Counting {
        handled: std::sync::atomic::AtomicUsize,
        refused: std::sync::Mutex<Vec<Vec<tars_types::CompatibilityReason>>>,
    }

    #[async_trait::async_trait]
    impl Middleware for Arc<Counting> {
        fn name(&self) -> &'static str {
            "counting"
        }
        async fn handle(
            &self,
            req: ChatRequest,
            ctx: Caller,
            next: Next<'_>,
        ) -> Result<LlmEventStream, OpenFailure> {
            self.handled
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            next.run(req, ctx).await
        }
        fn on_refusal(&self, refusal: &RefusedCall<'_>) {
            let LlmError::Incompatible { reasons } = refusal.error() else {
                panic!(
                    "a refusal is told with its Incompatible error: {}",
                    refusal.error()
                );
            };
            self.refused.lock().unwrap().push(reasons.clone());
        }
    }

    #[tokio::test]
    async fn a_refused_call_is_told_to_every_layer_and_handled_by_none() {
        let counting = Arc::new(Counting::default());
        let inner = MockProvider::new(ProviderId::new("profiled"), CannedResponse::text("ok"));
        let profile = tars_types::ProviderProfile::nothing_claimed(tars_types::InterfaceKind::Http);
        let svc = LlmService::builder(Arc::new(Profiled { inner, profile }), "m")
            .layer(Arc::clone(&counting))
            .build();

        let mut thinking = ChatRequest::user("ping");
        thinking.thinking = tars_types::ThinkingMode::Auto;
        let refused = svc.call(thinking, Caller::test_default()).await.result;
        assert!(matches!(refused, Err(LlmError::Incompatible { .. })));
        drop(
            svc.call(ChatRequest::user("ping"), Caller::test_default())
                .await
                .result
                .unwrap(),
        );

        assert_eq!(
            counting.handled.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the request the provider can serve reaches the layer"
        );
        let told = counting.refused.lock().unwrap();
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            matches!(
                told[0].as_slice(),
                [tars_types::CompatibilityReason::ThinkingUnsupported { .. }]
            ),
            "{told:?}"
        );
    }

    #[tokio::test]
    async fn unknown_limit_sends_none() {
        let mock = MockProvider::new(ProviderId::new("svc_test"), CannedResponse::text("ok"));
        let svc = LlmService::of(mock, "mock-1");
        assert_eq!(svc.output_limit(), &OutputLimit::Unknown);
    }

    #[tokio::test]
    async fn empty_service_passes_through_to_provider() {
        let mock = MockProvider::new(ProviderId::new("svc_test"), CannedResponse::text("ok"));
        let svc = LlmService::of(mock, "mock-1");

        let req = ChatRequest::user("ping");
        let mut stream = svc.call(req, Caller::test_default()).await.result.unwrap();

        let mut got_finished = false;
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream yielded a provider error");
            if matches!(ev, ChatEvent::Finished { .. }) {
                got_finished = true;
            }
        }
        assert!(got_finished);
    }

    #[test]
    fn a_stream_that_reached_finished_ends_without_a_further_error() {
        assert!(unfinished(true, false).is_none());
    }

    #[test]
    fn a_stream_that_ended_before_finished_ends_unfinished() {
        assert!(matches!(
            unfinished(false, false),
            Some(ProviderError::Unfinished)
        ));
    }

    #[test]
    fn a_stream_that_failed_keeps_its_own_error() {
        assert!(unfinished(false, true).is_none());
    }

    async fn items_of(events: Vec<ChatEvent>) -> Vec<Result<ChatEvent, ProviderError>> {
        let mock = MockProvider::new(
            ProviderId::new("svc_test"),
            CannedResponse::Sequence(events),
        );
        let stream = LlmService::of(mock, "mock-1")
            .call(ChatRequest::user("ping"), Caller::test_default())
            .await
            .result
            .unwrap();
        stream.collect().await
    }

    #[tokio::test]
    async fn a_provider_stream_without_finished_reaches_the_caller_as_unfinished() {
        let items = items_of(vec![
            ChatEvent::started("m"),
            ChatEvent::Delta {
                text: "half an ans".into(),
            },
        ])
        .await;
        // The provider time goes out just before the error the stream ends in.
        assert!(
            matches!(
                items.as_slice(),
                [
                    Ok(ChatEvent::Started { .. }),
                    Ok(ChatEvent::Delta { .. }),
                    Ok(ChatEvent::Measured { telemetry }),
                    Err(ProviderError::Unfinished),
                ] if telemetry.provider_latency_ms.is_some()
            ),
            "{items:?}"
        );
    }

    #[tokio::test]
    async fn a_provider_stream_with_finished_reaches_the_caller_whole() {
        let items = items_of(vec![
            ChatEvent::started("m"),
            ChatEvent::Finished {
                stop_reason: tars_types::StopReason::EndTurn,
                usage: tars_types::Usage::default(),
            },
        ])
        .await;
        // The provider time goes out just before `Finished`.
        assert!(
            matches!(
                items.as_slice(),
                [
                    Ok(ChatEvent::Started { .. }),
                    Ok(ChatEvent::Measured { telemetry }),
                    Ok(ChatEvent::Finished { .. }),
                ] if telemetry.provider_latency_ms.is_some()
            ),
            "{items:?}"
        );
    }

    #[tokio::test]
    async fn an_attempt_that_fails_to_open_is_timed() {
        let mock = MockProvider::new(
            ProviderId::new("svc_test"),
            CannedResponse::Error("down".into()),
        );
        let svc = LlmService::of(mock, "mock-1");
        let r = svc
            .run(
                &CallLedger::default(),
                ChatRequest::user("ping"),
                Caller::test_default(),
            )
            .await;
        let Err(failed) = r else {
            panic!("a provider that is down opens no stream");
        };
        assert!(failed.measured.provider_latency_ms.is_some());
    }
}
