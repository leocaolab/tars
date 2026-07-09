# Doc 02 — Middleware Pipeline and Request Lifecycle

> Scope: defines the full lifecycle of an LLM request from Runtime entry until response (or rejection), and the Middleware Pipeline abstraction that carries this flow.
>
> Upstream: consumes the `LlmProvider` trait defined in Doc 01.
>
> Downstream: invoked by the Agent Runtime (Doc 04); carries all cross-cutting concerns spanning business logic.

---

## 1. Design Goals

| Goal | Description |
|---|---|
| **Decoupled concerns** | IAM / Cache / Budget / Guard each have independent implementation, testing, and replacement |
| **Configurable order** | Onion-layer order is expressed explicitly in config; security-sensitive layers (IAM) can be locked to a fixed position |
| **Stream-friendly** | Every middleware must handle streams correctly; "buffer the entire stream then forward" is not allowed as a default implementation |
| **Cancel-safe** | Cancel signals from upper-layer Drop must propagate down to the Provider layer; Doc 01 §6.2.1's CLI interrupt mechanism depends on this |
| **Multi-tenant isolation** | Every layer perceives tenant_id via RequestContext; tenant config overrides global defaults |
| **Short-circuitable** | Any layer may decide "do not continue" and return a result or error directly |
| **Observable** | Every layer's entry/exit has an OTel span; every decision point has a queryable event |

**Anti-goals**:
- No prompt assembly / RAG retrieval / Agent orchestration in the Middleware layer — these are upper-layer responsibilities
- No cross-request mutable business state held by middleware — state is externalized to Cache Registry / Budget Store / etc.
- No hidden retry inside a business middleware — retry is one explicit layer; provider *fallback* is a caller composition (try one service, on error try the next), not a layer

---

## 2. Architecture Overview

```
                  ┌─────────────────────────────────────┐
                  │  Agent Runtime / Application Layer  │
                  └──────────────────┬──────────────────┘
                                     │ ChatRequest + RequestContext
                                     ▼
   ┌─────────────────────────────────────────────────────────────┐
   │  ▼ inbound                                       outbound ▲ │
   │ ┌─────────────────────────────────────────────────────────┐ │
   │ │  L1  Telemetry        (outermost, wraps everything)     │ │
   │ │ ┌─────────────────────────────────────────────────────┐ │ │
   │ │ │  L2  Auth & IAM    (before Cache, cannot bypass)    │ │ │
   │ │ │ ┌─────────────────────────────────────────────────┐ │ │ │
   │ │ │ │  L3  Budget Control                            │ │ │ │
   │ │ │ │ ┌─────────────────────────────────────────────┐ │ │ │ │
   │ │ │ │ │  L4  Cache Lookup                          │ │ │ │ │
   │ │ │ │ │ ┌─────────────────────────────────────────┐ │ │ │ │ │
   │ │ │ │ │ │  L5  Prompt Guard (Fast + Slow lane)   │ │ │ │ │ │
   │ │ │ │ │ │ ┌─────────────────────────────────────┐ │ │ │ │ │ │
   │ │ │ │ │ │ │  L6  Retry                         │ │ │ │ │ │ │
   │ │ │ │ │ │ │ ┌─────────────────────────────────┐ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │  CircuitBreaker (provider wrap) │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ ┌─────────────────────────────┐ │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ │   LlmProvider call          │ │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ └─────────────────────────────┘ │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ └─────────────────────────────────┘ │ │ │ │ │ │ │
   │ │ │ │ │ │ └─────────────────────────────────────┘ │ │ │ │ │ │
   │ │ │ │ │ └─────────────────────────────────────────┘ │ │ │ │ │
   │ │ │ │ └─────────────────────────────────────────────┘ │ │ │ │
   │ │ │ └─────────────────────────────────────────────────┘ │ │ │
   │ │ └─────────────────────────────────────────────────────┘ │ │
   │ └─────────────────────────────────────────────────────────┘ │
   └─────────────────────────────────────────────────────────────┘
```

`L1..L6` are `Middleware` layers added with `.layer(...)`; the
**CircuitBreaker is not a layer** — it wraps the single `LlmProvider`
(applied below Retry), so it is drawn just above the provider but is
composed differently (see §4.7).

**Inbound ordering principles**:
1. **Telemetry outermost** — spans must wrap all failure and short-circuit paths
2. **Auth and IAM next** — any "optimization that bypasses IAM" is a security hole
3. **Budget before Cache** — when budget is exhausted, even cache lookups should not happen (minor optimization, but cleaner semantics)
4. **Cache before Guard** — a cache-hit request already passed Guard validation in the past; no need to repeat
5. **Guard before Retry** — whether a prompt is legal has no bearing on how many times we retry the provider
6. **Retry → Circuit Breaker → Provider** — Retry is the innermost *middleware*; the circuit breaker is a wrapper around the single `LlmProvider` sitting *below* Retry, so an open breaker rejects each attempt before the provider is hit and Retry reacts to that rejection

> **Provider *selection* is not a pipeline concern.** There is no
> Routing / Ensemble / Fallback layer — those were removed by decision.
> A caller who wants several providers composes several `LlmService`s
> itself: ensemble = build N services, call all, merge; fallback = try
> one, on error try the next. The pipeline's single primitive is "one
> provider + one bound model, wrapped in a middleware chain".

**Outbound ordering** (naturally the reverse of inbound):
- L6 Retry decides whether to try again
- L4 Cache writes the successful response (async, doesn't block return)
- L3 Budget debits actual consumption (based on Usage)
- L1 Telemetry closes the span and emits final metrics

---

## 3. Core Abstractions

### 3.1 `LlmService` — the one concrete service

`LlmService` is **not** a trait — it is the single public, concrete
service struct: **one provider + one bound model + an ordered list of
`Middleware` layers**. There is no service trait, no per-layer wrapper
service, and no `dyn LlmService`. Business code holds an `LlmService` and
calls `svc.call(req, ctx)`; it is model-blind — the concrete model is
bound here at construction, never carried on the `ChatRequest`.

```rust
pub struct LlmService {
    provider: Arc<dyn LlmProvider>,     // the single terminal provider
    model: String,                      // concrete model, bound here
    layers: Vec<Arc<dyn Middleware>>,   // outermost-first
}

impl LlmService {
    pub async fn call(
        &self,
        req: ChatRequest,               // pure content — carries no model
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError>;

    pub fn model(&self) -> &str;
    pub fn provider(&self) -> &Arc<dyn LlmProvider>;
    pub fn layer_names(&self) -> &[&'static str];   // outermost-first
}
```

Calling it drives the layers as a **handler chain**: each layer gets the
request and a `Next` cursor, does its pre-work, calls `next.run(req, ctx)`
— zero times to short-circuit (cache hit, budget reject), once normally,
many times to retry — then post-processes. The terminal of the chain is
`provider.stream(req, model, ctx)`. `LlmService` is cheap to clone (Arcs +
a small Vec of Arcs).

### 3.2 `Middleware` — the handler-chain trait

A middleware is ONE type with ONE method. It does its pre-work, calls
`next.run(...)` to descend to the next layer (or the terminal provider),
then post-processes the result / stream — the same shape as a
`tower::Service` wrapping its inner, but driven by an explicit `next`
cursor rather than a stored `inner` handle. There is no `Middleware::wrap`
and no tower `Layer`.

```rust
#[async_trait]
pub trait Middleware: Send + Sync + 'static {
    /// Stable, low-cardinality label for tracing spans / metrics.
    fn name(&self) -> &'static str;

    /// Handle one call: pre-work, then `next.run(req, ctx)` (0×, 1×, or
    /// N×), then post-work on the result / stream.
    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError>;
}
```

`Next` is a `Copy` cursor over the remaining chain. A layer calls
`next.run(req, ctx)` to advance; a layer that keys on the bound model
reads it from `next.model()` (cache key, telemetry label, event record) —
the ones that don't never see it. **The model rides on the cursor**, not
on the request or the context.

Pipeline construction goes through the builder — the **first** `.layer(...)`
ends up OUTERMOST, the provider innermost:

```rust
let svc = LlmService::builder(provider, "claude-sonnet-5")
    .layer(TelemetryMiddleware::new())   // outermost
    .layer(RetryMiddleware::default())   // closest to the provider
    .build();                            // -> LlmService
```

`LlmService::default_chain(provider, model, opts)` assembles the canonical
onion in one call; `chain_over(inner, opts)` / `builder_with_inner(inner)`
wrap an already-built service as the inner stack under additional outer
layers. The caller obtains an `LlmService` whose external shape is
identical to a single Provider — all middleware is transparent to the
caller.

### 3.3 RequestContext

```rust
pub struct RequestContext {
    pub trace_id: TraceId,
    pub tenant_id: TenantId,
    pub session_id: SessionId,
    pub principal: Principal,                  // caller identity
    pub deadline: Option<Instant>,             // deadline for the entire request
    pub cancel: CancellationToken,             // tokio_util CancellationToken
    pub budget: BudgetHandle,                  // snapshot of currently available budget
    pub attributes: HashMap<String, Value>,    // free-form extension slot
}
```

**Key design points**:
- `cancel` is the core of cooperative cancellation. Any layer may listen on `cancel.cancelled()`, and any layer may call `cancel.cancel()` to actively terminate. Doc 01 §6.2.1's CLI cancel guard is implemented by subscribing to this token.
- `budget` is a reference, not a copy — multiple concurrent requests share the same BudgetHandle, and debits are atomic.
- `attributes` is intentionally retained — to avoid forcing changes to RequestContext fields whenever middleware needs to pass custom state to each other.

### 3.4 Short-circuit return

Some middleware decides during inbound to "stop here and return directly" — e.g. IAM denial, Cache hit, Guard interception. Short-circuiting is implemented by returning a **pre-built stream**:

```rust
pub fn short_circuit_with_response(
    response: ChatResponse,
) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
    let events = response.into_events();
    Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
}

pub fn short_circuit_with_error(
    err: ProviderError,
) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
    Err(err)
}
```

When short-circuiting, **inner service is not called**, and outer middleware cannot detect the difference — what they see is a normal event stream (or an error). This preserves abstraction consistency.

---

## 4. Detailed Design of Each Layer

### 4.1 L1 — Telemetry

**Responsibility**: establish the OTel root span for the entire request; create child spans for each Middleware; convert streaming events into metrics.

```rust
pub struct TelemetryMiddleware {
    tracer: Arc<dyn Tracer>,
    metrics: Arc<TelemetryMetrics>,
}

#[async_trait]
impl Middleware for TelemetryMiddleware {
    fn name(&self) -> &'static str { "telemetry" }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError> {
        let span = self.tracer.start_span("llm.request")
            .with_attribute("tenant", ctx.tenant_id.as_str())
            .with_attribute("trace_id", ctx.trace_id.as_str())
            .with_attribute("model", next.model());   // model lives on the service
        
        let start = Instant::now();
        let metrics = self.metrics.clone();
        
        let inner_stream = next.run(req, ctx).await
            .map_err(|e| {
                span.record_error(&e);
                metrics.record_error(&e);
                e
            })?;
        
        // Stream wrapping: observe each event, track TTFT, token rate, final usage
        let mut first_token_emitted = false;
        let stream = inner_stream.inspect(move |event| {
            match event {
                Ok(ChatEvent::Delta { .. }) | Ok(ChatEvent::ThinkingDelta { .. }) => {
                    if !first_token_emitted {
                        metrics.record_ttft(start.elapsed());
                        first_token_emitted = true;
                    }
                }
                Ok(ChatEvent::Finished { usage, stop_reason }) => {
                    metrics.record_usage(usage);
                    metrics.record_total_latency(start.elapsed());
                    span.record_attribute("stop_reason", stop_reason.as_str());
                    span.end();
                }
                Err(e) => {
                    metrics.record_error(e);
                    span.record_error(e);
                }
                _ => {}
            }
        });
        
        Ok(Box::pin(stream))
    }
}
```

**Required metrics** (must be collected):
- `llm.ttft_ms` (time to first token)
- `llm.total_latency_ms`
- `llm.tokens.input` / `llm.tokens.output` / `llm.tokens.cached`
- `llm.cost_usd`
- `llm.stop_reason` (label)
- `llm.errors` (labeled by ErrorClass)

### 4.2 L2 — Auth & IAM

**Responsibility**: verify caller identity; decide whether the caller is authorized to operate on the resources referenced by the current request (tenant, session, referenced code repos).

```rust
pub struct AuthMiddleware {
    authenticator: Arc<dyn Authenticator>,
    iam_engine: Arc<dyn IamEngine>,
}

#[async_trait]
impl Middleware for AuthMiddleware {
    fn name(&self) -> &'static str { "auth" }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError> {
        // 1. Authentication: who is the principal?
        if !self.authenticator.verify(&ctx.principal).await? {
            return short_circuit_with_error(ProviderError::Auth("invalid principal".into()));
        }
        
        // 2. Authorization: can the principal access the resources referenced by req?
        let resources = req.referenced_resources();   // e.g. "repo:tars", "session:xyz"
        let decision = self.iam_engine.evaluate(&ctx.principal, &resources, "llm:invoke").await?;
        
        if !decision.allowed {
            // Must intercept before entering Cache Lookup (Doc 03 §IAM precedence)
            return short_circuit_with_error(ProviderError::Auth(format!(
                "denied: {}", decision.reason
            )));
        }
        
        // 3. Record the IAM decision in ctx.attributes so later layers can read it
        let mut ctx = ctx;
        ctx.attributes.insert("iam.allowed_scopes".into(), decision.scopes.into());
        
        next.run(req, ctx).await
    }
}
```

**Hard invariants**:
1. The IAM decision must complete before Cache Lookup. Any optimization of the form "check the cache first and authorize later" is an IDOR vulnerability — see Doc 03.
2. IAM failures are always `ErrorClass::Permanent`, never retried.
3. The IAM decision result (allowed scopes, visible projects) is written to `ctx.attributes`; the Cache layer uses it to construct namespace-isolated hash factors.

### 4.3 L3 — Budget Control

**Responsibility**: check budget before the request leaves; accumulate actual consumption during streaming; actively cancel on exhaustion.

Budget has three tiers:
- **RPM / TPM**: requests per minute / tokens per minute (rate limiting, prevents instantaneous bursts)
- **Daily quota**: total daily allowance (cost control)
- **Cost ceiling**: monetary upper bound (final defense)

```rust
pub struct BudgetMiddleware {
    store: Arc<dyn BudgetStore>,                 // Redis implementation
    estimator: Arc<TokenEstimator>,
}

#[async_trait]
impl Middleware for BudgetMiddleware {
    fn name(&self) -> &'static str { "budget" }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError> {
        // 1. Pre-reserve: occupy budget using estimated token count.
        //    The model is bound on the service — read it off the cursor.
        let estimated_input = self.estimator.estimate(&req);
        let estimated_output = req.max_output_tokens.unwrap_or(2048) as u64;
        let estimated_cost = estimate_cost(next.model(), estimated_input, estimated_output);
        
        let reservation = self.store.reserve(
            &ctx.tenant_id,
            BudgetReservation {
                request_id: ctx.trace_id.clone(),
                tokens: estimated_input + estimated_output,
                cost_usd: estimated_cost,
            },
        ).await?;
        
        if !reservation.granted {
            return short_circuit_with_error(ProviderError::BudgetExceeded);
        }
        
        // 2. Call downstream
        let inner_stream = match next.run(req, ctx.clone()).await {
            Ok(s) => s,
            Err(e) => {
                // Release reservation immediately on failure
                self.store.release(&reservation).await.ok();
                return Err(e);
            }
        };
        
        // 3. Stream tracking: accumulate tokens, cancel if mid-stream overage
        let store = self.store.clone();
        let cancel = ctx.cancel.clone();
        let stream = inner_stream.inspect(move |event| {
            if let Ok(ChatEvent::UsageProgress { partial }) = event {
                // Some providers report usage mid-stream
                if partial.output_tokens > reservation.tokens * 12 / 10 {
                    // Actual consumption exceeds reservation by 20% → cancel
                    cancel.cancel();
                }
            }
            if let Ok(ChatEvent::Finished { usage, .. }) = event {
                // Final settlement: release reservation + debit by actual usage
                let actual_cost = compute_cost(&usage);
                tokio::spawn({
                    let store = store.clone();
                    let reservation = reservation.clone();
                    async move {
                        store.commit(&reservation, actual_cost).await.ok();
                    }
                });
            }
        });
        
        Ok(Box::pin(stream))
    }
}
```

**Key design points**:
- **Two-phase reserve + settle**: avoids "discovering overage halfway through streaming". Reservation is pessimistic (uses max_output_tokens); the actual amount usually doesn't fully consume it.
- **Mid-stream overage**: if a provider supports `UsageProgress` events (OpenAI and Gemini partially do), abnormal overage can be detected during generation (e.g. max=2048 but already emitted 3000) and cancelled early.
- **Token estimation uses fast mode** (`chars / 4`); no tokenizer is loaded on the request path — Doc 01 §15 anti-pattern 1.
- **Commit runs asynchronously**: doesn't block stream return; debit failures are logged and alerted but no more.

### 4.4 L4 — Cache Lookup

**Responsibility**: look up an existing response by IAM-hardened cache key; short-circuit on hit; on miss, continue and asynchronously write back after the response arrives.

Detailed implementation in Doc 03; this layer only describes the interface to the Pipeline:

```rust
#[async_trait]
impl Middleware for CacheLookupMiddleware {
    fn name(&self) -> &'static str { "cache_lookup" }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError> {
        // 1. Build namespace-isolated cache key (depends on scopes written by
        //    previous IAM layer). The key is model-aware — the model is bound
        //    on the service, read via `next.model()`, not off the request.
        let scopes = ctx.attributes.get("iam.allowed_scopes")
            .ok_or_else(|| ProviderError::Internal("iam scopes missing".into()))?;
        let key = self.compute_key(&req, next.model(), &ctx.tenant_id, scopes);
        
        // 2. L1 in-memory lookup
        if let Some(cached) = self.l1.get(&key).await {
            return short_circuit_with_response(cached);
        }
        
        // 3. L2 Redis lookup
        if let Some(cached) = self.l2.get(&key).await? {
            self.l1.put(key.clone(), cached.clone()).await;
            return short_circuit_with_response(cached);
        }
        
        // 4. L3 Provider explicit cache (Gemini cachedContent / Anthropic cache_control)
        //    Inject into req.cache_directives; Provider layer is responsible for using it
        let mut req = req;
        if let Some(handle) = self.l3_lookup(&key, &ctx).await? {
            req.cache_directives.push(CacheDirective::UseExplicit { handle });
        }
        
        // 5. Call downstream
        let inner_stream = next.run(req, ctx).await?;
        
        // 6. Stream wrapping: accumulate response, async write to cache on completion
        let writer = self.writer.clone();
        let key_for_write = key.clone();
        let mut accumulator = ChatResponseBuilder::new();
        let stream = inner_stream.inspect(move |event| {
            if let Ok(ev) = event {
                accumulator.apply_ref(ev);
                if matches!(ev, ChatEvent::Finished { .. }) {
                    let response = accumulator.snapshot();
                    let writer = writer.clone();
                    let key = key_for_write.clone();
                    tokio::spawn(async move {
                        writer.write(key, response).await.ok();
                    });
                }
            }
        });
        
        Ok(Box::pin(stream))
    }
}
```

### 4.5 L5 — Prompt Guard (dual lane)

**Responsibility**: intercept prompt injection, jailbreak instructions, sensitive content. The previously discussed fast/slow dual lane lands in this layer.

```rust
pub struct PromptGuardMiddleware {
    fast: Arc<FastGuard>,                        // aho-corasick
    slow: Arc<dyn ClassifierProvider>,           // ONNX DeBERTa
    config: GuardConfig,
}

#[async_trait]
impl Middleware for PromptGuardMiddleware {
    fn name(&self) -> &'static str { "prompt_guard" }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError> {
        // 1. Extract text to inspect: only user input, not system / history (avoid wasted work)
        let text_to_check = extract_user_input(&req);
        
        // 2. Fast lane: serial, must complete in <1ms
        if self.fast.scan(&text_to_check) {
            return short_circuit_with_error(ProviderError::ContentFiltered {
                category: "fast_heuristic".into(),
            });
        }
        
        // 3. Slow lane: launch in parallel, race against the downstream LLM call
        let slow = self.slow.clone();
        let slow_check = tokio::spawn(async move {
            slow.classify(&text_to_check).await
        });
        
        // 4. Launch downstream
        let inner_stream = next.run(req, ctx.clone()).await?;
        
        // 5. select! pattern: two legs running in parallel
        //    - slow lane returns unsafe first → cancel inner stream + return ContentFiltered
        //    - inner stream finishes naturally → slow lane result is used for audit but doesn't affect return
        let cancel = ctx.cancel.clone();
        let stream = async_stream::try_stream! {
            tokio::pin!(slow_check);
            let mut inner = inner_stream;
            let mut slow_resolved = false;
            
            loop {
                tokio::select! {
                    biased;  // prioritize checking the slow lane
                    
                    result = &mut slow_check, if !slow_resolved => {
                        slow_resolved = true;
                        match result {
                            Ok(Ok(classification)) if classification.is_unsafe() => {
                                // Intercept: cancel downstream, short-circuit return error
                                cancel.cancel();
                                Err(ProviderError::ContentFiltered {
                                    category: format!("ml_classifier:{}", classification.label),
                                })?;
                            }
                            _ => continue,  // safe or classification failure → continue with inner
                        }
                    }
                    
                    Some(event) = inner.next() => {
                        yield event?;
                    }
                    
                    else => break,
                }
            }
        };
        
        Ok(Box::pin(stream))
    }
}
```

**Key decisions**:
- **Fast lane serial + slow lane parallel** — hides the 10-30ms ML inference behind the LLM TTFT; legitimate requests pay zero security latency (the optimization mentioned in the discussion)
- **Intercepted requests waste tens of tokens of prefill** — an acceptable cost
- **Role-context separation**: classify with an attached `role_hint` ("code review / doc generation / free-form chat") so the classifier can calibrate. Avoids the false positive of "user-submitted malicious-looking code samples being misjudged".
- **Slow lane failures don't block**: if the classifier is down, degrade to "fast lane only", alert but don't trip business

### 4.6 Provider selection is not a layer (removed)

Earlier designs put a **Routing** layer here that picked a `ProviderId`
from a `ModelHint` + policy and rewrote the request's model. That layer
was **removed by decision**: provider *selection* is not a pipeline
concern, and the request no longer carries a model at all.

- **One service = one provider + one bound model.** The `ModelHint →
  concrete model` resolution happens *before* the service is built (at
  role resolution / config time — Doc 01 §12); by the time the pipeline
  runs, the model is fixed and passed to `provider.stream(req, model, ctx)`
  as an explicit argument, read by layers that need it via `next.model()`.
- **Ensemble / fallback are caller compositions, not layers.** A caller
  that wants several providers builds several `LlmService`s and combines
  them itself: ensemble = call all, merge; fallback = try one, on error
  try the next. Keeping that out of the onion means every layer below
  stays "one provider" simple, and the breaker/retry accounting is exact.

So the chain goes straight from Prompt Guard to **Retry** (§4.8), with the
**Circuit Breaker** (§4.7) wrapping the single provider beneath it.

### 4.7 Circuit Breaker (a provider wrapper, not a layer)

**Responsibility**: track the bound Provider's health; trip the circuit
after a run of failures and fail fast for a cooldown instead of hammering a
down provider.

The breaker is **not a `Middleware`** and is **not** added with `.layer(...)`.
It decorates the single `Arc<dyn LlmProvider>` — applied *below* Retry, so
an open breaker rejects each attempt before the provider is hit and Retry
reacts to that rejection:

```rust
// Applied by `default_chain` when `ChainOpts::circuit_breaker` is set:
let provider = CircuitBreaker::wrap(provider, cfg);   // Arc<dyn LlmProvider>
let svc = LlmService::of(provider, model);            // then the onion goes on top
```

```rust
#[async_trait]
impl LlmProvider for CircuitBreaker {
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        model: &str,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        if self.state() == CircuitState::Open {
            // Reject without touching the inner provider. `CircuitOpen` is a
            // Retriable class, so an outer RetryMiddleware reacts to it.
            return Err(ProviderError::CircuitOpen { retry_after: self.cooldown_remaining() });
        }
        let out = self.inner.clone().stream(req, model, ctx).await;
        self.record(&out);   // advance the breaker state machine
        out
    }
}
```

Because the breaker state lives on the single wrapper, **every call routed
through the same built service shares it** — concurrent fan-out callers
fast-fail the moment any of them trips it.

**Configuration** (`CircuitBreakerConfig`):
- `failure_threshold`: consecutive open-time failures before it opens
- `cooldown`: how long it stays open before probing again

### 4.8 Retry

**Responsibility**: retry recoverable errors against the bound provider.
There is **no provider fallback** here — a single service targets a single
provider; switching providers is a caller composition (§4.6), not this
layer's job.

```rust
#[async_trait]
impl Middleware for RetryMiddleware {
    fn name(&self) -> &'static str { "retry" }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError> {
        let mut attempt = 0;
        loop {
            if ctx.cancel.is_cancelled() {
                return Err(ProviderError::Internal("cancelled".into()));
            }
            // `Next` is Copy — call `next.run(...)` again to re-drive the
            // rest of the chain (down to the provider) for the next attempt.
            match next.run(req.clone(), ctx.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) => match e.class() {
                    ErrorClass::Retriable if attempt < self.config.max_attempts - 1 => {
                        attempt += 1;
                        let backoff = compute_backoff(attempt, e.retry_after());
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    _ => return Err(e),   // Permanent, or attempts exhausted
                },
            }
        }
    }
}
```

**Key decisions**:
- **Permanent errors are never retried** — 4xx / content filter / budget exhaustion / context too long — a retry would fail the same way
- **Backoff algorithm**: exponential + jitter + respects `retry_after` hints
- **Streaming responses are not retried mid-stream** — if the stream has already emitted several tokens and then fails, it must be passed up to the caller (partial result + error); silent retry would cause duplicate content
- **An open circuit breaker** (§4.7) surfaces as a `Retriable` `CircuitOpen`, so Retry backs off exactly as it would for any transient error

### 4.9 Outbound layers (Schema Validation, etc.)

Schema Validation is conceptually an "outbound" layer, but in implementation it is still a Middleware — it wraps the inner stream and validates the complete response when the stream finishes:

```rust
#[async_trait]
impl Middleware for ValidationMiddleware {
    fn name(&self) -> &'static str { "validation" }

    async fn handle(
        &self,
        req: ChatRequest,
        ctx: RequestContext,
        next: Next<'_>,
    ) -> Result<LlmEventStream, ProviderError> {
        let schema = match &req.structured_output {
            Some(s) => s.clone(),
            None => return next.run(req, ctx).await,  // no validation needed, pass-through
        };
        
        let inner_stream = next.run(req, ctx.clone()).await?;
        
        let mut accumulator = TextAccumulator::new();
        let validator = self.validator.clone();
        
        let stream = async_stream::try_stream! {
            let mut inner = inner_stream;
            while let Some(event) = inner.next().await {
                let event = event?;
                if let ChatEvent::Delta { text } = &event {
                    accumulator.push(text);
                }
                if let ChatEvent::Finished { .. } = &event {
                    // Stream finished, validate complete text
                    let full = accumulator.into_string();
                    match validator.validate(&full, &schema) {
                        Ok(_) => yield event,
                        Err(e) => Err(ProviderError::Parse(format!(
                            "schema validation failed: {}", e
                        )))?,
                    }
                } else {
                    yield event;
                }
            }
        };
        
        Ok(Box::pin(stream))
    }
}
```

**Schema validation is usually redundant when Provider strict mode is already enabled** — but kept as defense in depth, and it is necessary for scenarios where the Provider does not support strict mode (local Ollama).

---

## 5. Cancel Signal Propagation

```
Application Layer
       │ creates RequestContext { cancel: CancellationToken::new() }
       ▼
Middleware L1 (Telemetry)
       │ inspects cancel, starts span
       ▼
... (middleware chain) ...
       │
       ▼
Middleware L5 (PromptGuard) ──── slow lane returns unsafe ────► cancel.cancel()
       │
       ▼
Middleware L6 (Retry) ───── stream.next() returns error due to cancel ───► return Err
       │
       ▼
LlmProvider (CLI backend)
       │ CancelGuard::drop() ──► send Interrupt JSONL ──► subprocess stops
```

Every layer must do two things:
1. **Pass the token**: hand `ctx.cancel.clone()` to the inner service
2. **Respond to the token**: long operations (including await stream.next()) pair with `select!` to listen for cancel

Failure mode: a layer performs a blocking operation (sync IO, blocking mutex) without listening for cancel — the entire chain stalls. In Rust this is expressed explicitly via `tokio::select!` and `CancellationToken::cancelled()`.

---

## 6. Special Challenges of Streaming

### 6.1 Mid-stream short-circuit patterns

Three typical short-circuit moments:
1. **Prompt Guard slow lane judges malicious** (§4.5) → select! pattern
2. **Budget mid-stream overage** (§4.3) → inspect + cancel.cancel()
3. **Timeout / upstream actively cancels** (application layer) → deadline + cancel

Unified principle: **short-circuit = cancel + return error event**, not directly closing the stream. This lets upper-layer inspect/observability still see a complete event sequence (including "why it ended early").

### 6.2 Observe vs consume

- **Telemetry / Cost Accounting**: observe events, forward unchanged (`inspect`)
- **Cache Store**: accumulate a copy and write to cache, forward unchanged (`inspect` + async spawn)
- **Schema Validation**: accumulate a copy, validate at the end, forward unchanged or replace with error
- **Prompt Guard**: may cancel + replace stream tail

Only Schema Validation **modifies the stream** in the abnormal case; the others are read-only observation. This distinction makes performance analysis easier: read-only layers are zero-copy; writing layers may introduce allocations.

### 6.3 Cost of stream wrapping

Every layer's `inspect` / `async_stream` introduces one BoxStream wrapping. On the hot path, 10 middleware layers means 10 heap allocations (the streams themselves) + N×event-count vtable calls.

In practice LLM streaming throughput is typically 50-200 events/s, and the wrapping cost is negligible. However:
- Don't do syscalls (file write, network, lock) inside inspect closures
- Don't do deep clones inside inspect closures
- Async tasks (cache write, metrics emit) all `tokio::spawn`, never blocking the stream

---

## 7. Configuration Shape

```toml
[pipeline]
# Order: outer to inner
order = [
  "telemetry",
  "auth",
  "iam",
  "budget",
  "cache_lookup",
  "prompt_guard",
  "schema_validation",      # outbound logic, but layer position is still on the outside
  "retry",                  # innermost middleware
]
# The circuit breaker is NOT a layer in this list — it wraps the single
# provider below Retry (`ChainOpts::circuit_breaker`, §4.7). There is no
# `routing` layer: provider selection is a caller composition (§4.6).

# Hard-locked positional constraints: violation causes startup failure
[pipeline.constraints]
"iam" = { must_be_before = ["cache_lookup"] }
"auth" = { must_be_before = ["iam"] }
"telemetry" = { must_be_outermost = true }

[middleware.budget]
store = "redis"
default_tpm = 100000
default_rpm = 60
default_daily_cost_usd = 50

[middleware.cache_lookup]
l1_capacity = 1024
l1_ttl_secs = 300
l2_backend = "redis"
l3_enabled = true

[middleware.prompt_guard]
fast_lane = "regex"
fast_lane_patterns_file = "/etc/tars/guard_patterns.txt"
slow_lane = "onnx"
slow_lane_provider = "guard_classifier"
slow_lane_threshold = 0.85
slow_lane_mode = "parallel"        # vs "serial"

[middleware.retry]
max_retries = 3
backoff_initial_ms = 200
backoff_max_ms = 10000
jitter_ratio = 0.3

[middleware.circuit_breaker]
failure_threshold = 0.5
min_requests = 20
open_duration_secs = 30
half_open_max_requests = 3
```

**Tenant-level override**:

```toml
[tenants.acme_corp.middleware.budget]
default_tpm = 500000          # higher quota for large customers
default_daily_cost_usd = 500
```

---

## 8. Error Handling and Short-circuit Semantics

Each Middleware can return three kinds of result on inbound:
1. **Continue**: normally call `inner.call()` and return the result (stream)
2. **ShortCircuit success**: return a pre-built response stream (e.g. Cache hit)
3. **ShortCircuit error**: return Err (e.g. IAM denial, Budget exhaustion)

When inbound short-circuiting, **inner.call() is not called**, so inner middleware has no perception of it. Outbound short-circuiting (replacing with an error inside the stream) is visible to inner middleware.

General principles for error propagation:
- Permanent errors (4xx, IAM, content filter) → throw upward immediately, no retry, no switch
- Recoverable errors (5xx, network, timeout) → absorbed by the Retry layer (or surfaced to the caller, who may try another service)
- Errors inside the stream → emitted as `ChatEvent::Err`; the caller decides whether to surface them

---

## 9. Testing Strategy

### 9.1 Per-layer unit tests

Put the layer over a **mock provider** and drive the service — the layer
sees a real `Next` cursor, and the mock stands in for the terminal call:

```rust
#[tokio::test]
async fn iam_blocks_unauthorized() {
    let mock = MockProvider::new("p", CannedResponse::text("hi"));
    let svc = LlmService::builder(mock, "test-model")
        .layer(AuthMiddleware::new(MockAuthenticator::accept_all(), MockIam::deny_all()))
        .build();

    let result = svc.call(test_request(), test_ctx_unauthorized()).await;
    assert!(matches!(result, Err(ProviderError::Auth(_))));
}
```

### 9.2 Canonical-order tests

The onion order is fixed in `default_chain` (load-bearing — e.g. Validation
must sit outside Cache; see §7 / Doc 15 §2), not user-reorderable, so the
test asserts the assembled `layer_names()` match the documented order:

```rust
#[tokio::test]
async fn default_chain_matches_documented_onion() {
    let mut opts = ChainOpts::new(ProviderId::new("p"));
    opts.validators = vec![Arc::new(NotEmptyValidator::new()) as _];
    opts.events = Some(EventStores { events, records });
    let svc = LlmService::default_chain(mock, "test-model", opts);

    // Outermost → innermost.
    assert_eq!(
        svc.layer_names(),
        &["event_emitter", "telemetry", "validation", "cache_lookup", "retry"],
    );
}
```

### 9.3 End-to-end integration tests

Full pipeline + MockProvider, asserting on event-stream shape, ordering, and metrics output:

```rust
#[tokio::test]
async fn cache_hit_short_circuits_provider() {
    let provider_call_count = Arc::new(AtomicU32::new(0));
    let mock_provider = MockProvider::counting(provider_call_count.clone());
    
    let pipeline = build_test_pipeline(mock_provider);
    
    // First call - miss, hits provider
    pipeline.call(req.clone(), ctx.clone()).await?.collect::<Vec<_>>().await;
    assert_eq!(provider_call_count.load(Ordering::SeqCst), 1);
    
    // Second call - hit, must not call provider
    pipeline.call(req, ctx).await?.collect::<Vec<_>>().await;
    assert_eq!(provider_call_count.load(Ordering::SeqCst), 1);
}
```

### 9.4 Cancel propagation tests

```rust
#[tokio::test]
async fn cancel_propagates_to_provider() {
    let provider_cancelled = Arc::new(AtomicBool::new(false));
    let mock = MockProvider::observing_cancel(provider_cancelled.clone());
    
    let pipeline = build_test_pipeline(mock);
    let ctx = test_ctx();
    let cancel = ctx.cancel.clone();
    
    let mut stream = pipeline.call(req, ctx).await?;
    let _ = stream.next().await;        // grab the first event
    cancel.cancel();                     // actively cancel
    drop(stream);                        // Drop triggers Provider cancel
    
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(provider_cancelled.load(Ordering::SeqCst));
}
```

---

## 10. Anti-pattern Checklist

1. **Don't hold cross-request mutable state in middleware** — externalize it (Cache Store, Budget Store, Metrics Registry).
2. **Don't perform syscalls on the hot path** (except Cache lookup and metrics emit, both of which must be async).
3. **Don't ignore the cancel signal** — long operations must use select! together with cancel.cancelled().
4. **Don't buffer the entire stream then forward in a streaming middleware** — except for cases like Schema Validation that fundamentally need the complete text. Even then, consider incremental validation.
5. **Don't call the Provider directly from within a middleware** — descend through `next.run()` so outer layers (Retry, Telemetry, …) can observe and intervene. The one exception is the circuit breaker, which is a provider *wrapper*, not a middleware (§4.7).
6. **Don't let IAM decisions depend on Cache data** — Cache is a performance optimization, not a security boundary.
7. **Don't retry Permanent errors in the retry middleware** — wastes quota and may trigger provider-side abuse detection.
8. **Don't let the Telemetry layer handle business logic** — observe only, don't decide. Telemetry failures must never affect the business path.
9. **Don't have multiple middlewares each parse the same response** — accumulate a copy once and share it across layers (pass a ResponseAccumulator handle via ctx.attributes).
10. **Don't put provider *selection* inside a middleware** — a service binds one provider + one model by construction; ensemble / fallback across providers is a caller composition (§4.6), never a layer.
11. **Don't create a new reqwest Client / DB connection per request** — reuse Arc instances; pools are initialized at Middleware construction time.
12. **Don't leave inter-middleware dependencies implicit** ("this layer assumes the previous one wrote some field to ctx.attributes") — document them or express via a typed Extension container.

---

## 11. Boundaries with Upstream and Downstream

### Upstream (Agent Runtime) contract

When the Agent Runtime calls the pipeline, it commits to:
- Provide a complete RequestContext (trace_id, tenant_id, principal, cancel token)
- Set a reasonable deadline
- Drop the stream when no longer needed (triggers cancel propagation)

### Downstream (Provider) contract

When the Pipeline calls a Provider, it commits to:
- ChatRequest has already gone through prompt assembly, IAM check, and Guard
- the model is handed to the provider as an **explicit argument** —
  `provider.stream(req, model, ctx)`; the `ChatRequest` carries no model
- Will not retry requests for which the Provider has returned a Permanent error

### Inter-middleware contracts (via ctx.attributes)

| Key | Writer | Reader |
|---|---|---|
| `iam.allowed_scopes` | L2 Auth | L4 Cache Lookup |
| `cache.hit` | L4 Cache | L1 Telemetry (used as a metric label) |
| `budget.reservation_id` | L3 Budget | L3 Budget (on outbound commit) |

When adding new middleware, register the attributes it reads/writes in the docs to avoid implicit dependencies.

---

## 12. TODOs and Open Questions

- [ ] Choice of dependency-injection container at Pipeline startup (hand-rolled vs `shaku` / `dependency-inversion`)
- [ ] Hot-reload mechanism for multi-tenant config (apply new quotas without restarting the service)
- [ ] Implementation details of the Budget Store on Redis (Lua-script atomic debit vs WATCH/MULTI)
- [ ] Auto-retry feedback loop for schema validation failures (currently we throw; in the future we could feed it back into the LLM for self-repair)
- [ ] Protocol choice between the Telemetry layer and the OpenTelemetry Collector (OTLP gRPC vs HTTP)
- [ ] Pipeline metrics exposure format (Prometheus pull vs OTel push)
