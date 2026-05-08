# Doc 01 — LLM Provider Abstraction Design

> Scope: defines the unified LLM Provider abstraction layer, supporting OpenAI / Anthropic / Gemini across both API and CLI channels, plus local inference engines (vLLM, llama.cpp, embedded mistral.rs, ONNX classifiers).
>
> Out of scope: upper-layer Agent orchestration, Cache Registry, Middleware Pipeline — these consume the Provider interface and are not defined at the Provider layer.

---

## 1. Design Goals

| Goal | Description |
|---|---|
| **Backend-agnostic** | Switching providers in upper-layer Agent code requires no business-logic changes, only config edits |
| **Capability negotiation** | Feature support varies wildly across providers (thinking, cache, tool use, structured output); a Capability descriptor lets upper layers make correct decisions |
| **CLI reuses local auth** | Invokes the user's already-logged-in `claude` / `gemini` CLI on the local machine; zero key management, zero ops |
| **Zero overhead for local backends** | Embedded inference (mistral.rs, ONNX) goes through the same abstraction but never touches the network stack |
| **Streaming and non-streaming unified** | Internally only stream exists; complete = stream + collect |
| **Errors are classifiable** | Distinguish retriable / non-retriable / content-filtered so upper-layer retry policy can be written |
| **Cancel-safe** | When the upper layer drops a stream, underlying resources (HTTP connections, CLI subprocess stdout buffers) must be cleaned up |

**Non-goals** (explicitly out):
- No prompt assembly, template rendering, or RAG retrieval at the Provider layer — those are upper-layer responsibilities
- No exposure of provider-specific concepts (`thinking_budget` is Anthropic-only and shouldn't appear in the generic trait)
- No conversation state in the trait — all calls are stateless, the upper layer passes the full message list
- No token-level precise budget control — that's Middleware's job; Provider only reports usage

---

## 2. Backend Matrix

Three integration categories, 9 typical backends in total:

| Category | Backend | Protocol | Auth | Tool Use | Structured Output | Prompt Cache | Streaming |
|---|---|---|---|---|---|---|---|
| HTTP API | OpenAI | REST/SSE | Bearer | ✅ strict mode | ✅ strict JSON Schema | implicit (>1024 prefix) | ✅ SSE |
| HTTP API | Anthropic | REST/SSE | x-api-key | ✅ tool use | via tool | ✅ explicit cache_control | ✅ SSE |
| HTTP API | Gemini | REST/SSE | API key / ADC | ✅ functionCalling | ✅ responseSchema | ✅ explicit cachedContent | ✅ SSE |
| HTTP API | OpenAI-compatible (vLLM / llama.cpp server / LM Studio / Groq / Together) | REST/SSE | none / Bearer | partial | partial | implementation-dependent | ✅ |
| HTTP API | Ollama | proprietary + OpenAI-compatible | none | ✅ | ✅ | ❌ | ✅ |
| CLI subprocess | Claude Code (`claude`) | JSONL over stdio | delegated (user is logged in) | ✅ | ✅ | delegated | ✅ |
| CLI subprocess | Gemini CLI (`gemini`) | text stdout | delegated | via MCP | partial | delegated | partial |
| Embedded | mistral.rs | in-process FFI | n/a | ✅ | ✅ | ❌ | ✅ |
| Embedded | ONNX Runtime (classification only) | in-process | n/a | ❌ | ❌ | ❌ | ❌ |

**Key observation**:
- vLLM / llama.cpp server / LM Studio / Groq / Together / DeepSeek all speak OpenAI-compatible protocol — meaning OpenAIProvider only needs `base_url` override support to cover them all, no need for N separate implementations.
- The ONNX backend doesn't fit the chat trait; it lives under a separate `ClassifierProvider` (used by PromptGuard).

---

## 3. Core Trait

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &ProviderId;
    fn capabilities(&self) -> &Capabilities;

    /// The single core method: returns an event stream
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError>;

    /// Default impl: consume the stream and aggregate
    async fn complete(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<ChatResponse, ProviderError> {
        let mut s = self.stream(req, ctx).await?;
        let mut acc = ChatResponseBuilder::new();
        while let Some(ev) = s.next().await {
            acc.apply(ev?);
        }
        acc.finish()
    }

    /// Estimate token count. fast=true allows returning an estimate; false calls the provider's count_tokens API
    async fn count_tokens(&self, req: &ChatRequest, fast: bool) -> Result<u64, ProviderError>;

    /// Compute cost (USD) from usage. Local backends return 0
    fn cost(&self, usage: &Usage) -> CostUsd;
}
```

### 3.1 On the `BoxStream<'static>` Lifetime Constraint

Returning `BoxStream<'static, ...>` means the stream **cannot borrow** `&self` while being polled — HTTP byte streams, SSE parsers, and MPSC receivers all get wrapped into this stream, and any provider-internal state must be moved in.

**Unified implementation pattern**: the trait takes `self: Arc<Self>`, all impls hold shareable state inside `Arc`. The first line of `stream` is `let this = self.clone();`, then `this` gets moved into the returned `async-stream` block:

```rust
impl LlmProvider for OpenAIProvider {
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let this = self.clone();
        let auth = this.auth_resolver.resolve(&this.config.auth, &ctx).await?;
        let url = this.build_url(&req);
        let body = this.translate_request(&req)?;
        
        // The whole reqwest call runs inside the stream body; reqwest::Response's lifetime is tied to the stream
        let stream = async_stream::try_stream! {
            let resp = this.http_client
                .post(url)
                .headers(this.build_headers(&auth))
                .json(&body)
                .send()
                .await
                .map_err(ProviderError::from)?;
            
            this.check_status(&resp)?;
            
            let mut sse = SseDecoder::new(resp.bytes_stream());
            let mut tool_buffer = ToolCallBuffer::new();   // see §8.1
            
            while let Some(frame) = sse.next().await {
                if let Some(event) = this.parse_event(frame?, &mut tool_buffer)? {
                    yield event;
                }
            }
        };
        
        Ok(Box::pin(stream))
    }
}
```

**Constraint**: every LlmProvider impl's internal state must satisfy "all operations remain valid after `Arc<Self>::clone()`" — i.e. any method available on `&self` must also be callable on `Arc<Self>`. reqwest::Client, AuthResolver, and config structs are all wrapped in Arc.

### 3.2 Key Decisions

- **stream is primitive, complete is derived** — avoids two code paths and avoids the "lost usage on streaming" bug class at the upper layer.
- **Item is `Result<ChatEvent, ProviderError>`** — not `ChatEvent`. Mid-stream errors (connection drop, malformed SSE) are normal; can't express them via panic.
- **No sync version exposed** — every provider is async, including embedded (mistral.rs inference itself is sync, wrapped in `spawn_blocking`).
- **`RequestContext` carries trace_id / tenant_id / budget** — separating business parameters (ChatRequest) from runtime metadata (RequestContext) lets the provider do tracing without polluting business fields.

---

## 4. Normalized Request / Response Model

### 4.1 ChatRequest

```rust
pub struct ChatRequest {
    pub model: ModelHint,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub structured_output: Option<JsonSchema>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub stop_sequences: Vec<String>,
    pub seed: Option<u64>,
    pub cache_directives: Vec<CacheDirective>,
    pub thinking: ThinkingMode,
}

pub enum ModelHint {
    Explicit(String),
    Tier(ModelTier),
    Ensemble(Vec<ModelHint>),
}

pub enum ModelTier {
    Reasoning,      // top-tier (Opus / o1 / Gemini Pro)
    Default,        // workhorse (Sonnet / 4o / Flash)
    Fast,           // routing / classification (4o-mini / Haiku / Flash-8B)
    Local,          // local (Qwen / Llama)
}

pub enum Message {
    User { content: Vec<ContentBlock> },
    Assistant { 
        content: Vec<ContentBlock>, 
        tool_calls: Vec<ToolCall>,
    },
    Tool { 
        tool_call_id: String, 
        content: Vec<ContentBlock>,
        is_error: bool,
    },
}

pub enum ContentBlock {
    Text(String),
    Image { mime: String, data: ImageData },
}
```

**ModelHint is the most important design choice in the abstraction**: upper-layer Agent code writes `ModelTier::Fast` instead of `"gpt-4o-mini"` — the same code can run against Anthropic, OpenAI, or local models simply by switching routing config, no code changes.

### 4.2 ChatEvent (streaming events)

```rust
pub enum ChatEvent {
    /// Metadata: actual model selected, cache hit info
    Started { actual_model: String, cache_hit: CacheHitInfo },
    
    /// Text delta
    Delta { text: String },
    
    /// Thinking trace (Anthropic thinking / OpenAI o1 reasoning)
    ThinkingDelta { text: String },
    
    /// Three-phase tool call + index for parallel-call aggregation
    ToolCallStart { index: usize, id: String, name: String },
    ToolCallArgsDelta { index: usize, args_delta: String },
    ToolCallEnd { index: usize, id: String, parsed_args: serde_json::Value },
    
    /// Some providers report usage mid-stream
    UsageProgress { partial: PartialUsage },
    
    /// Terminal event
    Finished { 
        stop_reason: StopReason, 
        usage: Usage,
    },
}

pub enum StopReason {
    EndTurn,
    MaxTokens,                    // hit max_output_tokens; not an error, upper layer can trigger continue
    StopSequence(String),
    ToolUse,
    ContentFilter,
    Cancelled,                    // upper-layer initiated abort (CLI backend emits this on Drop signal)
    Other(String),
}
```

**Tool call design specifics in §8.1** — not just three-phase, but must aggregate by `index` to handle parallel calls.

---

## 5. Capability Descriptor

```rust
pub struct Capabilities {
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
    
    pub supports_tool_use: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_structured_output: StructuredOutputMode,
    pub supports_vision: bool,
    pub supports_thinking: bool,
    pub supports_cancel: bool,                 // CLI backend implements via interrupt signal
    
    pub prompt_cache: PromptCacheKind,
    pub streaming: bool,
    
    pub modalities_in: HashSet<Modality>,
    pub modalities_out: HashSet<Modality>,
    
    pub pricing: Pricing,
}

pub enum StructuredOutputMode {
    None,
    JsonObjectMode,               // OpenAI legacy json_object (no schema validation)
    StrictSchema,                 // OpenAI strict / Gemini responseSchema
    ToolUseEmulation,             // Anthropic via tool
}

pub enum PromptCacheKind {
    None,
    ImplicitPrefix { min_tokens: u32 },   // OpenAI
    ExplicitMarker,                       // Anthropic cache_control
    ExplicitObject,                       // Gemini cachedContent (separate API)
    Delegated,                            // CLI backend
}
```

Upper-layer Routing and Middleware decide based on this:
- Need strict JSON output → filter out providers with `StructuredOutputMode::None`
- Long system prompt reuse → prefer `ExplicitMarker` or `ExplicitObject`
- Vision tasks → filter on `modalities_in.contains(Image)`

---

## 6. Backend Implementation Categories

### 6.1 HTTP API Backends

Share a common base `HttpProviderBase`:
- Singleton `reqwest::Client` (connection pool shared across providers, wrapped in Arc)
- Common retry (exponential backoff + jitter)
- Common timeouts (separate connect / read / total)
- SSE parser
- OpenTelemetry injection

Each provider only implements:

```rust
trait HttpAdapter: Send + Sync + 'static {
    fn build_url(&self, model: &str) -> Url;
    fn build_headers(&self, auth: &ResolvedAuth) -> HeaderMap;
    fn translate_request(&self, req: &ChatRequest) -> serde_json::Value;
    fn parse_event(&self, raw: &SseFrame, buf: &mut ToolCallBuffer) -> Result<Option<ChatEvent>>;
    fn classify_error(&self, status: StatusCode, body: &[u8]) -> ProviderError;
}
```

OpenAIProvider, via `base_url`, simultaneously serves vLLM / llama.cpp server / LM Studio / Groq / Together / DeepSeek. When the peer doesn't support certain fields (e.g. early vLLM versions don't support strict json schema), the capability flags inform the upper layer; the provider layer never silently downgrades.

### 6.2 CLI Subprocess Backends

**Claude Code CLI is the gold standard for current CLI backends** — it has a real bidirectional JSONL protocol:

```bash
claude --print \
       --output-format stream-json \
       --input-format stream-json \
       --include-partial-messages
```

stdin is fed JSONL user messages, stdout emits a JSONL event stream. Architecture:

```rust
pub struct ClaudeCliProvider {
    config: Arc<ClaudeCliConfig>,
    sessions: Arc<DashMap<SessionId, Arc<ClaudeSession>>>,
}

struct ClaudeSession {
    child: Mutex<Child>,                            // tokio::process::Child
    stdin_tx: mpsc::Sender<InboundMessage>,
    event_dispatcher: broadcast::Sender<ChatEvent>,
    last_used: AtomicInstant,
    /// Currently-consuming request, used to locate target on cancel
    active_request: Mutex<Option<RequestId>>,
    janitor: JoinHandle<()>,
}
```

**Key decisions**:
- **Long-lived subprocess, not one-shot** — spawn takes ~200-500ms, unacceptable for interactive response latency
- **One process per SessionId** — messages within a session share conversation history; isolation across sessions
- **Janitor proactively kills after 5 min idle (default)** — prevents process leaks
- **stdout is parsed continuously by a dedicated reader task** that dispatches via broadcast channel
- **Auth fully delegated**: no API key in config, the CLI uses the credentials from the user's `claude login`. Multi-tenant scenarios require maintaining isolated `HOME` / `XDG_CONFIG_HOME` sandbox dirs per tenant.

#### 6.2.1 Cancel Safety (the core difficulty of CLI backends)

HTTP backend cancel is implicit: upper-layer drops stream → reqwest::Response is dropped → TCP connection closes → server infers termination. Clean.

**The CLI backend is dangerous**: dropping the stream upstream does not stop the `claude` subprocess that's currently generating tokens. If unhandled, the consequences are:

1. The subprocess keeps spitting stdout → blocks on pipe write → next session reuse reads stale leftovers, garbling responses
2. Force-killing the subprocess loses the resident-process benefit and pays the 200-500ms cold-start cost on every cancel

**Correct implementation**:

```rust
impl ClaudeCliProvider {
    async fn stream(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatEvent, ProviderError>>, ProviderError> {
        let this = self.clone();
        let session = this.acquire_session(&ctx.session_id).await?;
        let request_id = RequestId::new();
        
        // Register active request; cancel guard fires cleanup when stream is dropped
        *session.active_request.lock().await = Some(request_id);
        let cancel_guard = CancelGuard::new(session.clone(), request_id);
        
        let mut event_rx = session.event_dispatcher.subscribe();
        session.stdin_tx.send(InboundMessage::user_turn(req)).await?;
        
        let stream = async_stream::try_stream! {
            // cancel_guard moves into the closure, dropped when stream is dropped
            let _guard = cancel_guard;
            
            loop {
                match event_rx.recv().await {
                    Ok(event) if event.belongs_to(request_id) => {
                        let is_terminal = matches!(event, ChatEvent::Finished { .. });
                        yield event;
                        if is_terminal { break; }
                    }
                    Ok(_) => continue,                  // not for this request, drop
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        Err(ProviderError::Internal("event lag".into()))?;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        
        Ok(Box::pin(stream))
    }
}

/// Triggered when stream is dropped: send interrupt to subprocess, clear active_request marker
struct CancelGuard {
    session: Arc<ClaudeSession>,
    request_id: RequestId,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        let session = self.session.clone();
        let request_id = self.request_id;
        
        tokio::spawn(async move {
            let mut active = session.active_request.lock().await;
            if *active != Some(request_id) {
                return;  // already terminated naturally, nothing to do
            }
            
            // 1. Send JSONL control message to tell claude to interrupt the current turn
            //    (claude --output-format stream-json supports {"type":"interrupt"})
            let _ = session.stdin_tx.try_send(InboundMessage::Interrupt);
            
            // 2. Wait for the subprocess to emit a Cancelled event (with timeout)
            //    The reader task drains all leftover stdout until a Finished event arrives
            //    Until then the active_request marker is held; subsequent requests wait
            tokio::time::timeout(
                Duration::from_secs(3),
                session.wait_for_cancel_ack(request_id),
            ).await.ok();
            
            *active = None;
        });
    }
}
```

**Extra safety nets**:
- `tokio::process::Command::new("claude").kill_on_drop(true)` — when the provider instance is dropped, all subprocesses die with it, eliminating leaks
- If the subprocess doesn't respond to interrupt within 3 seconds, the janitor escalates to `child.kill()`, then marks the session `Poisoned`; next acquire rebuilds it
- `acquire_session` blocks/queues when `active_request.is_some()`; concurrent use of the same session is disallowed

### 6.3 Embedded Backends

**MistralRsProvider**: wraps `mistral.rs` for in-process inference. The trait adapter layer translates ChatRequest into mistral.rs's `Request`, receives incremental results via mpsc channel, and translates back to `ChatEvent`. Model loading happens at provider construction, never on the request path.

**OnnxClassifierProvider**: **does not implement LlmProvider trait**; it has its own `ClassifierProvider` trait. Forcing the classifier to fit the chat abstraction would pollute the design.

```rust
#[async_trait]
pub trait ClassifierProvider: Send + Sync {
    async fn classify(&self, text: &str) -> Result<ClassificationResult>;
}
```

---

## 7. Auth Strategy

```rust
pub enum Auth {
    Inline(SecretString),
    Env { var_name: String },
    SecretManager { backend: SecretBackend, key: String },
    GoogleAdc { scope: Vec<String> },
    Delegate { 
        per_tenant_home: bool,        // multi-tenant isolation: each tenant gets its own HOME
    },
    None,
}
```

`AuthResolver` is the single entry point:

```rust
#[async_trait]
pub trait AuthResolver: Send + Sync {
    async fn resolve(&self, auth: &Auth, ctx: &RequestContext) 
        -> Result<ResolvedAuth, AuthError>;
}
```

The implementation handles secret caching, refresh (OAuth token expiry), and multi-tenant isolation. **Provider impls never read env vars directly** — everything goes through AuthResolver injection.

---

## 8. Tool Calling Format Normalization

| Provider | Field | Args Format | Parallel Support |
|---|---|---|---|
| OpenAI | `tool_calls[].function.arguments` | **JSON string** (needs second-pass parsing) | ✅ via `index` |
| Anthropic | `content[].input` | JSON object | ✅ via `index` |
| Gemini | `functionCall.args` | JSON object | ✅ |

Normalized to:

```rust
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,   // always a parsed object, never a string
}
```

### 8.1 Streaming Aggregation of Parallel Tool Calls

OpenAI's / Anthropic's streaming args deltas **may interleave across indexes** — empirically they're mostly serial, but the spec allows event sequences like:

```
ToolCallStart   { index: 0, name: "search" }
ToolCallStart   { index: 1, name: "fetch" }
ToolCallArgsDelta { index: 0, args_delta: "{\"q" }
ToolCallArgsDelta { index: 1, args_delta: "{\"u" }
ToolCallArgsDelta { index: 0, args_delta: "uery\":\"x\"}" }
ToolCallArgsDelta { index: 1, args_delta: "rl\":\"y\"}" }
```

Provider-internal parsing buffer:

```rust
pub struct ToolCallBuffer {
    inflight: HashMap<usize, ToolCallAccumulator>,
}

struct ToolCallAccumulator {
    id: String,
    name: String,
    args_chunks: String,
}

impl ToolCallBuffer {
    pub fn on_start(&mut self, index: usize, id: String, name: String) {
        self.inflight.insert(index, ToolCallAccumulator { 
            id, name, args_chunks: String::new() 
        });
    }
    
    pub fn on_delta(&mut self, index: usize, delta: &str) {
        if let Some(acc) = self.inflight.get_mut(&index) {
            acc.args_chunks.push_str(delta);
        }
    }
    
    /// Called at stream end or on explicit ToolCallEnd: parse + fallback repair
    pub fn finalize(&mut self, index: usize) -> Result<(String, String, serde_json::Value)> {
        let acc = self.inflight.remove(&index)
            .ok_or(ProviderError::Parse(format!("no tool call at index {}", index)))?;
        
        // Three-phase fallback
        let parsed = serde_json::from_str::<serde_json::Value>(&acc.args_chunks)
            .or_else(|_| {
                // partial-json repair (close braces / quotes on truncated JSON)
                partial_json::fix_and_parse(&acc.args_chunks)
            })
            .map_err(|e| ProviderError::Parse(format!(
                "tool call args parse failed for {}: {} | raw: {}", 
                acc.name, e, acc.args_chunks
            )))?;
        
        Ok((acc.id, acc.name, parsed))
    }
}
```

**Key**: parsing only happens at `finalize`, never incrementally during deltas (delta-stage args are guaranteed incomplete JSON; incremental parsing wastes CPU and produces false errors).

---

## 9. Structured Output Normalization

```rust
pub struct JsonSchema {
    pub schema: serde_json::Value,
    pub strict: bool,
}
```

Per-provider translation:
- **OpenAI**: `response_format = { type: "json_schema", json_schema: { strict: true, schema: ... } }`
- **Gemini**: `responseSchema = ...` + `responseMimeType = "application/json"`
- **Anthropic**: inject a hidden tool `__respond_with__`, set `tool_choice = { type: "tool", name: "__respond_with__" }`, pass schema as input_schema. On response, treat the tool_use input as the final output.
- **vLLM**: supports `guided_json` / `guided_regex`, capability equivalent to OpenAI strict
- **llama.cpp**: supports `json_schema`, also guided generation
- **Local backends without guided support**: capability marked `StructuredOutputMode::None`, upper layer falls back to retry-with-error-feedback

---

## 10. Prompt Cache Abstraction

```rust
pub enum CacheDirective {
    /// Auto-cached (OpenAI implicit)
    Auto,
    
    /// Mark cache boundary at the current position (Anthropic); content before this point is cached
    MarkBoundary { ttl: Duration },
    
    /// Reference an already-created cache object (Gemini cachedContent)
    UseExplicit { handle: ProviderCacheHandle },
}

pub struct ProviderCacheHandle {
    pub provider: ProviderId,
    pub external_id: String,         // server-side cache handle, never exposed to user
    pub tenant_namespace: String,    // enforced tenant isolation
    pub created_at: SystemTime,
    pub expires_at: SystemTime,
}
```

Providers expose a separate sub-trait for managing explicit caches:

```rust
#[async_trait]
pub trait ExplicitCacheProvider: LlmProvider {
    async fn create_cache(&self, content: CacheableContent, ttl: Duration) 
        -> Result<ProviderCacheHandle>;
    async fn delete_cache(&self, handle: &ProviderCacheHandle) -> Result<()>;
    async fn extend_ttl(&self, handle: &ProviderCacheHandle, additional: Duration) 
        -> Result<()>;
}
```

Not every provider implements this sub-trait — OpenAI only uses `CacheDirective::Auto`.

### 10.1 Active Reclamation (cannot rely on TTL)

Cloud-provider cachedContent is billed by storage + input tokens; TTL is just a safety net. **Real cost control requires active delete**.

Who triggers delete: **the Cache Lookup Middleware maintains a global LRU + janitor task**; the Provider layer only exposes `delete_cache` capability and does not schedule on its own.

Trigger conditions:
1. **Session idle**: upper-layer Session times out (default 15 min no activity) → find all ProviderCacheHandles referenced by that session → batch delete
2. **LRU eviction**: global cache count or aggregate storage exceeds threshold → evict oldest by last_used_at
3. **Tenant switch**: user/tenant explicitly ends workflow → delete immediately
4. **Budget alert**: daily cache storage cost exceeds threshold → forced cleanup

Pseudocode (in Middleware layer, not Provider layer):

```rust
pub struct CacheJanitor {
    registry: Arc<CacheRegistry>,                  // session_id -> Vec<ProviderCacheHandle>
    providers: Arc<ProviderRegistry>,
    config: JanitorConfig,
}

impl CacheJanitor {
    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            self.evict_idle_sessions().await;
            self.evict_lru_if_over_quota().await;
        }
    }
    
    async fn evict_idle_sessions(&self) {
        let cutoff = Instant::now() - self.config.session_idle_timeout;
        let stale = self.registry.find_idle_since(cutoff);
        
        for (session_id, handles) in stale {
            for handle in handles {
                if let Some(provider) = self.providers.get_explicit_cache(&handle.provider) {
                    if let Err(e) = provider.delete_cache(&handle).await {
                        tracing::warn!(?e, ?handle, "cache delete failed; will retry");
                        continue;
                    }
                    self.registry.remove(&session_id, &handle);
                }
            }
        }
    }
}
```

**Why Middleware, not Provider**: the Provider layer is a stateless adapter and shouldn't hold business knowledge like "which caches belong to which session". Provider only exposes capabilities; Middleware handles scheduling. This is consistent with the earlier "Provider doesn't know about IAM / Cache / Budget" boundary.

---

## 11. Error Normalization

```rust
pub enum ProviderError {
    Auth(String),
    RateLimited { retry_after: Option<Duration> },
    BudgetExceeded,
    InvalidRequest(String),
    ContentFiltered { category: String },
    
    /// Input exceeds model context window — permanent error, upper layer must truncate
    ContextTooLong { limit: u32, requested: u32 },
    
    ModelOverloaded,
    Network(Box<dyn std::error::Error + Send + Sync>),
    Parse(String),
    CliSubprocessDied { exit_code: Option<i32>, stderr: String },
    Internal(String),
}
```

### 11.1 ContextTooLong vs MaxTokens

The two "limit exceeded" cases are semantically completely different and must never be conflated:

| Case | Symptom | Classification | Upper-layer handling |
|---|---|---|---|
| Input too long | request prompt > `max_context_tokens` | `ProviderError::ContextTooLong` | permanent error; must summarize / sliding window / split |
| Output truncated | inference reaches `max_output_tokens` | `StopReason::MaxTokens` (**not an error**) | can continue the turn; append Assistant message and trigger continue |

**HTTP Adapter responsibility**: many providers return 400 + a vague "context_length_exceeded" error when the prompt is too long; the adapter must recognize this and map it to `ContextTooLong { limit, requested }`, not throw `InvalidRequest`. Similarly, `finish_reason == "length"` in the response must map to `StopReason::MaxTokens`, not any error.

### 11.2 Error Classification

```rust
impl ProviderError {
    pub fn class(&self) -> ErrorClass {
        use ProviderError::*;
        match self {
            RateLimited { .. } | ModelOverloaded | Network(_) => ErrorClass::Retriable,
            
            Auth(_) | InvalidRequest(_) | ContextTooLong { .. } 
                | ContentFiltered { .. } | BudgetExceeded => ErrorClass::Permanent,
            
            Parse(_) | Internal(_) | CliSubprocessDied { .. } => ErrorClass::MaybeRetriable,
        }
    }
    
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            Self::ModelOverloaded => Some(Duration::from_secs(10)),
            _ => None,
        }
    }
}
```

The upper-layer retry Middleware decides based only on `class()`; no need to match concrete error variants.

---

## 12. Routing Layer

```rust
pub struct ProviderRegistry {
    providers: HashMap<ProviderId, Arc<dyn LlmProvider>>,
    routing: Arc<dyn RoutingPolicy>,
    metrics: Arc<ProviderMetrics>,
}

#[async_trait]
pub trait RoutingPolicy: Send + Sync {
    async fn select(
        &self,
        req: &ChatRequest,
        candidates: &[(ProviderId, Arc<dyn LlmProvider>)],
        metrics: &ProviderMetrics,
    ) -> Result<Vec<ProviderId>, RoutingError>;
}
```

Built-in policies:
- `ExplicitPolicy`: when req.model is `ModelHint::Explicit(name)`, dispatch directly
- `TierPolicy`: lookup by `ModelTier`
- `CostPolicy`: pick cheapest provider that meets capability requirements
- `LatencyPolicy`: pick fastest by P50 latency from metrics
- `EnsemblePolicy`: call multiple providers in parallel, merge by strategy (majority vote / fastest / longest)
- `FallbackChain<P>`: wraps another policy with automatic fallback on failure

Config example:

```toml
[routing]
default = "tier"

[routing.tiers]
reasoning = ["claude_opus_api", "openai_o1", "gemini_pro"]
default   = ["claude_sonnet_api", "openai_4o", "gemini_flash"]
fast      = ["openai_4o_mini", "claude_haiku", "local_qwen_7b"]
local     = ["local_qwen_32b"]

[routing.fallback]
on_rate_limit = "next_in_tier"
on_overload   = "next_in_tier"
on_auth       = "fail"
```

---

## 13. Configuration Shape

```toml
[providers.claude_api]
type = "anthropic"
base_url = "https://api.anthropic.com"
auth = { kind = "secret_manager", backend = "vault", key = "anthropic/prod" }
default_model = "claude-opus-4-7"
timeout = { connect_ms = 5000, read_ms = 120000 }

[providers.openai_api]
type = "openai"
auth = { kind = "env", var_name = "OPENAI_API_KEY" }
default_model = "gpt-4o"

[providers.local_qwen]
type = "openai_compat"
base_url = "http://ryzen-node-1:8000/v1"
auth = { kind = "none" }
default_model = "Qwen/Qwen2.5-Coder-32B-Instruct"
capabilities_override = { supports_thinking = false, prompt_cache = "none" }

[providers.claude_cli]
type = "claude_code_cli"
binary = "claude"
mode = "long_lived"
session_idle_timeout_secs = 300
auth = { kind = "delegate", per_tenant_home = true }

[providers.gemini_cli]
type = "gemini_cli"
binary = "gemini"
mode = "one_shot"
auth = { kind = "delegate", per_tenant_home = true }

[providers.embedded_qwen]
type = "mistral_rs"
model_path = "/models/qwen2.5-7b-q4.gguf"
gpu_layers = 999
context_size = 32768

[providers.guard_classifier]
type = "onnx_classifier"
model_path = "/models/deberta-injection-int8.onnx"
threshold = 0.85
```

---

## 14. Testing Strategy

Three-layer pyramid:

1. **Unit tests**: use `MockProvider` (replays pre-recorded responses from fixtures) to assert business logic. Fixtures use cassette-style recording of real provider responses, checked into git.
2. **Capability conformance tests**: every provider impl runs the same conformance suite (tool use / structured output / streaming / cancel / error classification) to verify the abstraction is leak-free. This layer runs nightly in CI against real APIs (cost-controlled, a few cents per run).
3. **Production smoke**: before each release, fire one minimal request per provider to confirm credentials and network reachability.

**Conformance tests must cover**:
- Mid-stream cancel (including session-reuse verification for CLI backends)
- Parallel tool call interleaved-index scenario (mock-injected; real models may not trigger it)
- Correct classification of ContextTooLong vs MaxTokens
- The full create / use / delete loop for explicit cache

---

## 15. Anti-pattern Checklist

1. **Don't do precise token counting on the request path**. Tokenizers load hundreds of MB and take 5-50ms per call. Use `chars / 4` estimation for budget checks; read the truth from response.usage.
2. **Don't implement streaming and non-streaming as two code paths**. Collecting a stream is a few lines of code; avoid doubling bug surface.
3. **Don't expose provider-specific fields in the trait**. `thinking_budget`, `prompt_cache_ttl`, `safety_settings` are all provider-specific; once they pollute the trait every provider has to fake-support them.
4. **Don't spawn a CLI process per request**. Claude CLI cold start is 200-500ms — disastrous for interactive latency.
5. **Don't assume the CLI binary is at a fixed path**. Use `which` lookup + config override.
6. **Don't pass OpenAI's args string up to callers as-is**. Provider-layer handles parse, repair, error reporting.
7. **Don't retry 4xx errors at the provider layer**. 401 / 400 / 422 are config or code bugs; retrying just burns quota.
8. **Don't drop CLI backend stderr**. CLI error info basically lives only in stderr; capture and stash into `ProviderError::CliSubprocessDied.stderr`.
9. **Don't drain all events before returning from `stream()`**. Return `BoxStream` and let the caller decide whether to aggregate; otherwise the streaming benefit is gone.
10. **Multi-tenant CLI backends must isolate HOME**. Otherwise tenant A's `claude login` overwrites tenant B's session and the permission boundary is broken.
11. **Don't classify `ContextTooLong` as `InvalidRequest`**. The former has clear handling paths upstream (truncate / summarize); the latter forces the caller to give up.
12. **Don't try JSON parsing during args delta phase**. Deltas are necessarily incomplete; only finalize at ToolCallEnd or stream termination.
13. **Don't assume stream Drop = automatic resource release** (only true for HTTP backends). CLI backends must cancel explicitly.
14. **Don't let the Provider hold business state like "this cache belongs to that session"**. Provider only exposes delete_cache; scheduling is Middleware's job.
15. **Don't rely on cloud-provider cache TTL for expiry**. Active delete is the only real cost control.

---

## 16. Boundary with Upper Layers (Middleware / Agent)

```
┌─────────────────────────────────────────┐
│  Agent Runtime (Orchestrator + Workers) │
└──────────────────┬──────────────────────┘
                   │ ChatRequest (with ModelHint)
                   ▼
┌─────────────────────────────────────────┐
│  Middleware Pipeline                    │
│  - IAM / Auth                           │
│  - Cache Lookup + Janitor (LRU active)  │
│  - Budget Control                       │
│  - Prompt Guard                         │
│  - Telemetry                            │
└──────────────────┬──────────────────────┘
                   │ ChatRequest (model resolved)
                   ▼
┌─────────────────────────────────────────┐
│  ProviderRegistry + RoutingPolicy       │
└──────────────────┬──────────────────────┘
                   │ Provider selected
                   ▼
┌─────────────────────────────────────────┐
│  LlmProvider impl                       │
│  - HTTP / CLI / Embedded                │
│  - Exposes capability only, no biz state│
└─────────────────────────────────────────┘
```

The Provider layer **is completely unaware** of Cache, IAM, Budget, or Tracing as business concepts — these are handled by Middleware before/after calling Provider. Provider's only job is "I'm a specific backend; give me a standard request, I emit a standard event stream, and I expose atomic capabilities like cache delete".

Once this boundary is clean, adding a new provider (e.g. xAI Grok ships an API) is just writing one `XaiAdapter` — all Cache / Budget / Routing behavior comes for free.

---

## 17. TODOs and Open Questions

- [ ] Validate mistral.rs embedded backend on macOS Apple Silicon Metal backend
- [ ] Confirm spec of Claude Code CLI's `interrupt` JSONL control message (may need adapters for multiple CLI versions)
- [ ] Evaluate maturity of Gemini CLI's stream protocol — worth promoting from one-shot to long-lived?
- [ ] OAuth token auto-refresh mechanism (Anthropic and Google have different flows)
- [ ] Multi-threaded inference scheduling for embedded ONNX classifier (avoid request contention on a single model instance)
