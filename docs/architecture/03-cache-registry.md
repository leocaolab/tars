# Doc 03 — Cache Registry

> Scope: defines the three-tier architecture of the LLM response cache (L1 in-process / L2 Redis / L3 Provider explicit cache), key construction, tenant isolation, stampede protection, and active reclamation (Janitor).
>
> Upstream: invoked by `CacheLookupMiddleware` in Doc 02 §4.4.
>
> Downstream: consumes the `ExplicitCacheProvider` sub-trait from Doc 01 §10 to manage Provider-side cache objects.

---

## 1. Design Goals

| Goal | Description |
|---|---|
| **Absolute tenant isolation** | Requests from different tenant_id / IAM scope must produce different cache keys, even with identical prompts |
| **Unified three-tier abstraction** | L1 / L2 / L3 expose the same interface to callers; differences are internal to the Registry |
| **Zero passthrough** | Singleflight ensures concurrent requests for the same key result in only one Provider call |
| **Active loss prevention** | Don't rely on Provider TTL auto-expiry; Janitor actively deletes to control cost |
| **Stream-friendly** | Cache writes execute asynchronously, never blocking response return |
| **Observable** | Every lookup / hit / miss / write / evict emits metrics |
| **Reversible** | No cache operation may corrupt data correctness — when the cache is broken, degrade to miss, never block business |

**Anti-goals**:
- No semantic caching (embedding similarity matching) — accuracy is uncontrollable, tenant isolation can't be done cleanly, leave it to upper-layer business
- Don't cache partial mid-stream responses — only cache complete successful responses
- Don't make IAM decisions in the Cache layer — the Cache Key already encodes IAM scope; Cache only recognizes Keys
- Don't expose L3 Provider cache raw handles to upper layers — must go through Registry wrapping

---

## 2. Three-Tier Cache Model

### 2.1 Layer Positioning

| Layer | Medium | Scope | Hit Latency | Typical TTL | Hit Benefit | Invalidation Cost |
|---|---|---|---|---|---|---|
| **L1** | In-process `moka` LRU | Single instance | <100µs | 5-15 min | Skip network entirely | Lost on process restart |
| **L2** | Redis | Cluster-shared | 1-3 ms | 1-24 h | Skip LLM API | Cross-instance sync cost |
| **L3** | Provider-side (Gemini cachedContent / Anthropic cache_control / OpenAI implicit) | Single Provider account | Full network round-trip but reduced input token billing | 5min - 1h | Significant input token cost reduction | Still pays output cost and latency |

**Key observations**:
- L1 and L2 cache the **complete response** — on hit, the Provider is not invoked
- L3 caches the **input prefix** — on hit, the Provider is still invoked, only input tokens aren't fully billed
- L3 must be explicitly managed (create, reference, delete); L1/L2 are transparent KV
- The three layers are not nested but complementary: L1/L2 hits skip the LLM, L3 hits halve cost but still call the LLM (suitable when each input differs but the system prompt is huge)

### 2.2 Selection Strategy

Not every request is suited to all three layers:

| Request profile | L1 | L2 | L3 |
|---|---|---|---|
| Short prompt + high-frequency repeats (FAQ) | ✅ | ✅ | ❌ unnecessary |
| Long system prompt + multi-turn dialog | ❌ multi-turn varies | ❌ same | ✅ system prompt reuse |
| Long RAG context + single inference | ❌ context unique | ❌ same | ✅ if context repeats |
| temperature > 0 creative generation | ❌ caching hurts diversity | ❌ same | ✅ still saves input cost |
| Tool use chained intermediate steps | ✅ strictly deterministic | ✅ | ❌ prefix typically short |

The strategy is expressed via `CachePolicy`, passed in with the request:

```rust
pub struct CachePolicy {
    pub l1: bool,
    pub l2: bool,
    pub l3: bool,
    pub l1_ttl: Option<Duration>,   // None = use default
    pub l2_ttl: Option<Duration>,
    pub l3_ttl: Option<Duration>,
}

impl Default for CachePolicy {
    fn default() -> Self {
        // L1+L2 on by default, L3 must be explicitly enabled per request
        Self { l1: true, l2: true, l3: false, l1_ttl: None, l2_ttl: None, l3_ttl: None }
    }
}
```

Callers (typically the Agent Runtime) pass it via `RequestContext::attributes`:

```rust
ctx.attributes.insert("cache.policy".into(), CachePolicy { l3: true, ..Default::default() }.into());
```

---

## 3. Cache Key Construction

### 3.1 The Fatal Anti-Example

The easiest IDOR vulnerability to introduce:

```rust
// ❌ Wrong: hash only the prompt
let key = sha256(format!("{}{}{}", system, user_msg, model));
```

This means: intern B and director A independently request a review of the same open-source code, and the cache keys are identical. If the code in some request was swapped for confidential code, B will read A's confidential analysis. **The core security issue discussed earlier**.

### 3.2 Correct Key Construction

```rust
pub struct CacheKey {
    pub fingerprint: [u8; 32],     // final SHA-256
    pub debug_label: String,        // logging only, not part of matching
}

pub struct CacheKeyFactory {
    hasher_version: u32,            // bumping this invalidates all caches at once
}

impl CacheKeyFactory {
    pub fn compute(
        &self,
        req: &ChatRequest,
        ctx: &RequestContext,
    ) -> Result<CacheKey, CacheError> {
        let mut h = Sha256::new();
        
        // —— isolation domain (must come first) ——
        h.update(self.hasher_version.to_le_bytes());
        h.update(b"\0TENANT\0");
        h.update(ctx.tenant_id.as_bytes());
        
        // IAM scopes must participate in the hash, with canonical sorting
        let scopes: &Vec<String> = ctx.attributes.get("iam.allowed_scopes")
            .ok_or(CacheError::MissingIamScopes)?
            .downcast_ref()
            .ok_or(CacheError::MissingIamScopes)?;
        let mut sorted_scopes = scopes.clone();
        sorted_scopes.sort();
        h.update(b"\0SCOPES\0");
        for scope in &sorted_scopes {
            h.update(scope.as_bytes());
            h.update(b"\0");
        }
        
        // —— model identity ——
        h.update(b"\0MODEL\0");
        match &req.model {
            ModelHint::Explicit(name) => h.update(name.as_bytes()),
            ModelHint::Tier(tier) => {
                // Tier cannot be hashed directly; must be resolved to Explicit by Routing first
                return Err(CacheError::UnresolvedModel);
            }
            ModelHint::Ensemble(_) => {
                // Ensemble typically should not be cached
                return Err(CacheError::UncacheableRequest);
            }
        }
        
        // —— output-determining request parameters ——
        h.update(b"\0PARAMS\0");
        // When temperature > 0, output theoretically differs each call → should not cache
        // But many callers don't explicitly set temperature; the default is Provider-decided
        // Policy: only cache when temperature is explicitly 0; skip everything else
        match req.temperature {
            Some(t) if t == 0.0 => h.update(b"t=0"),
            _ => return Err(CacheError::NonDeterministic),
        }
        if let Some(seed) = req.seed {
            h.update(b"\0SEED\0");
            h.update(seed.to_le_bytes());
        }
        if let Some(max) = req.max_output_tokens {
            h.update(b"\0MAX\0");
            h.update(max.to_le_bytes());
        }
        for stop in &req.stop_sequences {
            h.update(b"\0STOP\0");
            h.update(stop.as_bytes());
        }
        
        // —— content ——
        h.update(b"\0SYSTEM\0");
        if let Some(sys) = &req.system {
            h.update(sys.as_bytes());
        }
        
        h.update(b"\0MESSAGES\0");
        for msg in &req.messages {
            hash_message(&mut h, msg);   // canonical serialization
        }
        
        // Tools schema also affects output
        if !req.tools.is_empty() {
            h.update(b"\0TOOLS\0");
            let tools_canonical = serde_json::to_vec(&req.tools).map_err(CacheError::Serialize)?;
            h.update(&tools_canonical);
        }
        
        // Structured output schema
        if let Some(schema) = &req.structured_output {
            h.update(b"\0SCHEMA\0");
            let schema_canonical = serde_json::to_vec(&schema.schema).map_err(CacheError::Serialize)?;
            h.update(&schema_canonical);
        }
        
        let fingerprint: [u8; 32] = h.finalize().into();
        Ok(CacheKey {
            fingerprint,
            debug_label: format!(
                "tenant={} model={} msg_count={}",
                ctx.tenant_id, req.model.as_str(), req.messages.len()
            ),
        })
    }
}
```

**Mandatory rules**:
1. **`hasher_version`** is the first byte fed into the hash — bumping this constant invalidates all L1/L2 caches at once (used as emergency stop-loss when a hash bug is found)
2. **TENANT + SCOPES must come first** — they are logically the namespace; placing them up front aids debugging by raw-byte-stream localization
3. **Each field uses a `\0` separator + field-name tag** — prevents field concatenation ambiguity attacks ("abc" + "def" vs "ab" + "cdef")
4. **temperature ≠ 0 rejects caching outright** — caching non-deterministic output is meaningless
5. **ModelHint must be Explicit** — upstream Routing is responsible for resolution. Does this mean Cache Lookup must happen after Routing? No — Doc 02 places Cache before Routing. **Solution**: at cache lookup time, use only `ModelHint::Tier` to compute a "tenant-level hint key"; a hit requires that any Explicit model within the tenant's Tier can serve it; precise secondary matching is done after Routing. See §4.2

### 3.3 Message Canonicalization

```rust
fn hash_message(h: &mut Sha256, msg: &Message) {
    match msg {
        Message::User { content } => {
            h.update(b"\0USER\0");
            for block in content {
                hash_content_block(h, block);
            }
        }
        Message::Assistant { content, tool_calls } => {
            h.update(b"\0ASSISTANT\0");
            for block in content {
                hash_content_block(h, block);
            }
            // tool_calls must be sorted by id
            let mut sorted = tool_calls.clone();
            sorted.sort_by(|a, b| a.id.cmp(&b.id));
            for call in &sorted {
                h.update(b"\0TC\0");
                h.update(call.id.as_bytes());
                h.update(b"\0");
                h.update(call.name.as_bytes());
                h.update(b"\0");
                // arguments is JSON, must be canonical (keys sorted)
                let canonical = canonical_json(&call.arguments);
                h.update(&canonical);
            }
        }
        Message::Tool { tool_call_id, content, is_error } => {
            h.update(b"\0TOOL\0");
            h.update(tool_call_id.as_bytes());
            h.update(b"\0");
            h.update(if *is_error { b"E" } else { b"O" });
            for block in content {
                hash_content_block(h, block);
            }
        }
    }
}

fn hash_content_block(h: &mut Sha256, block: &ContentBlock) {
    match block {
        ContentBlock::Text(text) => {
            h.update(b"\0T\0");
            h.update(text.as_bytes());
        }
        ContentBlock::Image { mime, data } => {
            h.update(b"\0IMG\0");
            h.update(mime.as_bytes());
            h.update(b"\0");
            // Don't hash raw image bytes (potentially MB-scale); hash the SHA-256 digest
            h.update(data.content_hash());
        }
    }
}
```

**JSON canonicalization**: must use deterministic serialization (sorted keys, no whitespace, fixed numeric format), otherwise `{"a":1,"b":2}` and `{"b":2,"a":1}` produce different hashes. In the Rust ecosystem, `serde_json::to_string_pretty` is **not** acceptable — use the `canonical_json` crate or implement RFC 8785 yourself.

---

## 4. CacheRegistry Abstraction

### 4.1 Core trait

```rust
#[async_trait]
pub trait CacheRegistry: Send + Sync {
    /// Multi-tier lookup (L1 → L2)
    /// L3 is not in this interface; query L3 separately via lookup_l3_handle
    async fn lookup(
        &self,
        key: &CacheKey,
        policy: &CachePolicy,
    ) -> Result<Option<CachedResponse>, CacheError>;
    
    /// Write to L1 + L2
    async fn write(
        &self,
        key: CacheKey,
        response: CachedResponse,
        policy: &CachePolicy,
        metadata: WriteMetadata,
    ) -> Result<(), CacheError>;
    
    /// Look up an L3 handle (Provider-side cache) suited to the current request
    async fn lookup_l3_handle(
        &self,
        key: &CacheKey,
        provider: &ProviderId,
    ) -> Result<Option<ProviderCacheHandle>, CacheError>;
    
    /// Register a newly created L3 handle
    async fn register_l3_handle(
        &self,
        key: CacheKey,
        handle: ProviderCacheHandle,
        owner: SessionId,
    ) -> Result<(), CacheError>;
    
    /// Explicit invalidation (triggered by upstream business changes)
    async fn invalidate(&self, key: &CacheKey) -> Result<(), CacheError>;
    
    /// Tenant-level purge (compliance deletion, GDPR, etc.)
    async fn purge_tenant(&self, tenant: &TenantId) -> Result<u64, CacheError>;
}

pub struct CachedResponse {
    pub response: ChatResponse,
    pub cached_at: SystemTime,
    pub origin_provider: ProviderId,
    pub original_usage: Usage,        // original consumption, for "how much money saved" stats
}

pub struct WriteMetadata {
    pub session_id: SessionId,
    pub trace_id: TraceId,
}
```

### 4.2 Resolving the Two-Phase ModelHint Problem

Cache Lookup happens before Routing (per Doc 02's layer ordering); at this point `req.model` is still `ModelHint::Tier(...)`. Two-phase lookup:

```rust
async fn lookup(&self, key: &CacheKey, policy: &CachePolicy) 
    -> Result<Option<CachedResponse>, CacheError> 
{
    // Phase 1: query "responses from any model within this tenant" by Tier
    if let Some(cached) = self.l1.get_by_tier_key(&key.tier_fingerprint).await {
        // Hit condition: cached response originates from some Explicit model contained by the current Tier
        return Ok(Some(cached));
    }
    // ...
}
```

**Two independent hashes**:
- `tier_fingerprint`: computed using the Tier name ("reasoning" / "fast"), shared at Tier level within a tenant
- `explicit_fingerprint`: computed using the Explicit model, exact match

**Both are stored**. Lookup queries `tier_fingerprint` first and returns on hit (accepts a response from any model in the Tier); Write writes both keys simultaneously. This way:
- Whichever specific model Routing picks doesn't affect hit rate
- Requests across Tiers don't pollute each other (a reasoning cache won't be hit by fast)

Cost: cache size doubles. Given responses are typically 1-10KB, this is acceptable.

### 4.3 Multi-Tier Lookup Flow

```rust
impl CacheRegistry for DefaultCacheRegistry {
    async fn lookup(&self, key: &CacheKey, policy: &CachePolicy)
        -> Result<Option<CachedResponse>, CacheError>
    {
        // Health check: when cache is broken, degrade to miss; never block business
        let with_fallback = |result: Result<Option<CachedResponse>, CacheError>| {
            match result {
                Ok(v) => Ok(v),
                Err(e) => {
                    tracing::warn!(?e, ?key.debug_label, "cache lookup error, treating as miss");
                    self.metrics.record_lookup_error(&e);
                    Ok(None)
                }
            }
        };
        
        // L1
        if policy.l1 {
            if let Some(cached) = with_fallback(self.l1.get(key).await)? {
                self.metrics.record_hit(CacheLevel::L1, &cached);
                return Ok(Some(cached));
            }
        }
        
        // L2
        if policy.l2 {
            if let Some(cached) = with_fallback(self.l2.get(key).await)? {
                self.metrics.record_hit(CacheLevel::L2, &cached);
                // Backfill L1
                if policy.l1 {
                    self.l1.put(key.clone(), cached.clone(), policy.l1_ttl_or_default()).await;
                }
                return Ok(Some(cached));
            }
        }
        
        self.metrics.record_miss(key);
        Ok(None)
    }
}
```

**Key decisions**:
- **Cache errors must never contaminate business** — when Redis is down, lookup returns None, the request continues to the Provider; alert but don't block
- **L2 hits backfill L1** — next hit on the same instance skips Redis
- **Errors are also sampled** — `record_lookup_error` drives SRE attention

---

## 5. Cache Write Strategy

### 5.1 Write Timing

Only **complete successful** responses are written (`StopReason::EndTurn` / `StopReason::ToolUse`):

```rust
fn should_cache(response: &ChatResponse, policy: &CachePolicy) -> bool {
    if !policy.l1 && !policy.l2 { return false; }
    
    matches!(
        response.stop_reason,
        StopReason::EndTurn | StopReason::ToolUse
    )
    // Cases rejected from caching:
    // - MaxTokens (response was truncated, incomplete)
    // - Cancelled (upstream actively interrupted)
    // - ContentFilter (Provider refused to answer, meaningless)
    // - StopSequence (semantically incomplete)
    // - Other (unknown, conservatively skip caching)
}
```

### 5.2 Write Path (Async, Non-Blocking)

```rust
async fn write(
    &self,
    key: CacheKey,
    response: CachedResponse,
    policy: &CachePolicy,
    meta: WriteMetadata,
) -> Result<(), CacheError> {
    // L1 sync write (local memory, μs scale)
    if policy.l1 {
        self.l1.put(key.clone(), response.clone(), policy.l1_ttl_or_default()).await;
    }
    
    // L2 async write (avoid blocking response return)
    if policy.l2 {
        let l2 = self.l2.clone();
        let key = key.clone();
        let response = response.clone();
        let ttl = policy.l2_ttl_or_default();
        let metrics = self.metrics.clone();
        
        tokio::spawn(async move {
            if let Err(e) = l2.put(key.clone(), response, ttl).await {
                tracing::warn!(?e, ?key.debug_label, "l2 write failed");
                metrics.record_write_error(&e);
            }
        });
    }
    
    // L3 is not handled in the write path —— it is created by ExplicitCacheProvider on the request path,
    // and registered separately via register_l3_handle
    
    Ok(())
}
```

### 5.3 L3 (Provider Explicit Cache) Write Path

L3 is fundamentally different from L1/L2 — it is created during the Provider call, not written after the fact:

```rust
// Logic of upstream Cache Lookup Middleware (simplified)
async fn handle_l3(&self, req: &mut ChatRequest, key: &CacheKey, ctx: &RequestContext) {
    // 1. Check whether a reusable handle already exists
    let provider_id = ctx.attributes.get("routing.provider_priority")
        .and_then(|p| p.first());
    
    if let Some(handle) = self.registry.lookup_l3_handle(key, provider_id).await? {
        // Hit: inject directive; Provider call references it directly
        req.cache_directives.push(CacheDirective::UseExplicit { handle });
        return;
    }
    
    // 2. Miss: decide whether creation is worthwhile (heuristic)
    if !worth_creating_l3(req) {
        return;
    }
    
    // 3. Mark the request; Provider returns the newly created handle id in the response
    req.cache_directives.push(CacheDirective::MarkBoundary { 
        ttl: policy.l3_ttl_or_default() 
    });
    
    // 4. After response arrives, extract handle from ChatEvent::Started.cache_hit_info,
    //    and call register_l3_handle to register it
}

fn worth_creating_l3(req: &ChatRequest) -> bool {
    // L3 creation has cost (storage fees + creation latency); more isn't always better
    let prefix_size = estimate_prefix_tokens(req);
    prefix_size > 4096           // prefix too short — not worth it
        && req.tools.is_empty()  // tool schema changes too frequently
}
```

### 5.4 Streaming Write Accumulation

CacheLookupMiddleware inspects the inner stream to accumulate the response (already shown in Doc 02 §4.4); upon completion it calls `write()`:

```rust
let stream = inner_stream.inspect(move |event| {
    if let Ok(ev) = event {
        accumulator.apply_ref(ev);
        if matches!(ev, ChatEvent::Finished { .. }) {
            let response = accumulator.snapshot();
            if should_cache(&response, &policy) {
                let registry = registry.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    registry.write(key, response.into(), &policy, meta).await.ok();
                });
            }
        }
    }
});
```

**Note**: the accumulator must clone the original ChatEvent rather than hold a reference — when the stream is dropped, references become invalid.

---

## 6. Singleflight (Stampede Protection)

### 6.1 Problem Scenario

A hot request (e.g., 100 Agents simultaneously calling the same classification tool) → 100 concurrent misses → 100 Provider calls → 100x cost + rate limit triggered.

### 6.2 Singleflight Pattern

For concurrent requests on the same key, only one is allowed to reach downstream; others subscribe to the result of that in-flight operation:

```rust
pub struct SingleflightGate<T> {
    inflight: Arc<DashMap<CacheKey, broadcast::Sender<Result<T, ProviderError>>>>,
}

impl<T: Clone + Send + 'static> SingleflightGate<T> {
    /// Returns Either<Leader, Follower>
    /// Leader must do the actual work and broadcast the result
    /// Follower waits for the Leader's result
    pub fn enter(&self, key: CacheKey) -> SingleflightHandle<T> {
        match self.inflight.entry(key.clone()) {
            Entry::Occupied(e) => {
                // A leader already exists; this request is a follower
                SingleflightHandle::Follower {
                    rx: e.get().subscribe(),
                }
            }
            Entry::Vacant(e) => {
                // I'm first; become leader
                let (tx, _) = broadcast::channel(1);
                e.insert(tx.clone());
                SingleflightHandle::Leader {
                    tx,
                    key,
                    inflight: self.inflight.clone(),
                }
            }
        }
    }
}

pub enum SingleflightHandle<T> {
    Leader { 
        tx: broadcast::Sender<Result<T, ProviderError>>,
        key: CacheKey,
        inflight: Arc<DashMap<CacheKey, broadcast::Sender<Result<T, ProviderError>>>>,
    },
    Follower { 
        rx: broadcast::Receiver<Result<T, ProviderError>>,
    },
}

impl<T> Drop for SingleflightHandle<T> {
    fn drop(&mut self) {
        if let Self::Leader { key, inflight, .. } = self {
            inflight.remove(key);
        }
    }
}
```

### 6.3 Integration with the Lookup Flow

```rust
async fn lookup_or_compute<F, Fut>(
    &self,
    key: CacheKey,
    policy: CachePolicy,
    compute: F,
) -> Result<CachedResponse, ProviderError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<CachedResponse, ProviderError>>,
{
    // 1. Plain lookup
    if let Some(cached) = self.lookup(&key, &policy).await? {
        return Ok(cached);
    }
    
    // 2. Singleflight
    match self.gate.enter(key.clone()) {
        SingleflightHandle::Leader { tx, .. } => {
            // I run compute, broadcast the result
            let result = compute().await;
            
            // Write cache (only on success)
            if let Ok(ref response) = result {
                self.write(key, response.clone(), &policy, default_meta()).await.ok();
            }
            
            let _ = tx.send(result.clone().map_err(|e| e.clone()));
            result
        }
        SingleflightHandle::Follower { mut rx } => {
            // Wait for leader's result
            match tokio::time::timeout(Duration::from_secs(120), rx.recv()).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) | Err(_) => {
                    // Leader died or timed out → degrade, compute on our own
                    tracing::warn!("singleflight follower fallback to direct compute");
                    compute().await
                }
            }
        }
    }
}
```

### 6.4 Compatibility with Streaming Responses

The `compute` above returns a `CachedResponse` (complete response, not a stream). This means Singleflight **degrades streaming calls to non-streaming** — the follower obtains the complete response that the leader wrote into cache.

Cost: followers lose streaming TTFT advantages — they must wait for the leader to fully generate before getting the result.

Trade-off:
- Latency-sensitive and non-hot: skip singleflight; each request hits the Provider independently
- Cost-sensitive and hot: enable singleflight, accepting follower latency cost
- Default policy: automatically enable singleflight when in-flight count > 5 ("only enter protection when overwhelmed")

---

## 7. Provider-side Cache Lifecycle Management

### 7.1 Handle Registry (Content-Addressed + Reference Counted)

**Core semantics**: L3 handles are **content-addressed** — the same `CacheKey` on the same Provider is created exactly once and shared across multiple concurrent sessions. This is why §3.2 emphasizes that CacheKey must include TENANT + IAM_SCOPES: maximize reuse within isolation boundaries, enforce independence across them.

Cross-session sharing → **single-session ownership won't work**; lifecycle must be managed by reference counting:

```rust
pub struct L3HandleRegistry {
    /// (key, provider) -> handle record
    /// The same semantic key exists independently on different Providers
    by_key: Arc<DashMap<(CacheKey, ProviderId), Arc<L3HandleRecord>>>,
    
    /// Reverse index: all handle references held by a session (used to batch-release on session exit)
    by_session: Arc<DashMap<SessionId, HashSet<L3HandleId>>>,
    
    /// LRU index: sorted by last_used_at (used as quota eviction candidates)
    lru: Arc<Mutex<LruOrder>>,
    
    /// Persistent backend (Postgres) - prevents loss across process restarts
    persistent: Arc<dyn L3Persistence>,
}

pub struct L3HandleRecord {
    pub id: L3HandleId,
    pub provider: ProviderId,
    pub external_id: String,                  // Provider-side handle, e.g. "cachedContents/abc123"
    pub key: CacheKey,
    pub tenant: TenantId,
    pub size_estimate_bytes: u64,
    pub created_at: SystemTime,
    pub expires_at: SystemTime,
    
    /// Set of sessions currently referencing this handle
    /// Sharing semantics: same tenant + same IAM scopes + same content → same handle
    pub referencing_sessions: RwLock<HashSet<SessionId>>,
    
    pub last_used_at: AtomicSystemTime,
    pub usage_count: AtomicU64,
    
    /// Marked as pending deletion (entered after ref_count hits 0; awaits grace period)
    pub pending_eviction_since: RwLock<Option<SystemTime>>,
}

impl L3HandleRecord {
    pub fn ref_count(&self) -> usize {
        self.referencing_sessions.read().unwrap().len()
    }
}
```

### 7.2 Acquire / Release Protocol

```rust
impl L3HandleRegistry {
    /// Acquire or create a handle. On hit, increment reference count
    pub async fn acquire(
        &self,
        key: CacheKey,
        provider_id: &ProviderId,
        session: SessionId,
        creator: impl FnOnce() -> BoxFuture<'_, Result<ProviderCacheHandle, ProviderError>>,
    ) -> Result<L3HandleId, CacheError> {
        // 1. Look for existing handle
        if let Some(record) = self.by_key.get(&(key.clone(), provider_id.clone())) {
            // Share: bump ref, clear pending_eviction marker
            let mut sessions = record.referencing_sessions.write().unwrap();
            sessions.insert(session.clone());
            *record.pending_eviction_since.write().unwrap() = None;
            
            self.by_session.entry(session).or_default().insert(record.id.clone());
            self.metrics.record_l3_share_hit(&record);
            return Ok(record.id.clone());
        }
        
        // 2. Not found → singleflight create (prevents duplicate creation under concurrent acquires of the same key)
        let handle = self.creation_gate.run(key.clone(), creator).await?;
        
        let record = Arc::new(L3HandleRecord {
            id: L3HandleId::new(),
            provider: provider_id.clone(),
            external_id: handle.external_id,
            key: key.clone(),
            tenant: handle.tenant_namespace.parse().unwrap(),
            size_estimate_bytes: handle.size_estimate.unwrap_or(0),
            created_at: handle.created_at,
            expires_at: handle.expires_at,
            referencing_sessions: RwLock::new([session.clone()].into()),
            last_used_at: AtomicSystemTime::new(SystemTime::now()),
            usage_count: AtomicU64::new(0),
            pending_eviction_since: RwLock::new(None),
        });
        
        self.by_key.insert((key, provider_id.clone()), record.clone());
        self.by_session.entry(session).or_default().insert(record.id.clone());
        self.persistent.save(&record).await?;
        
        Ok(record.id.clone())
    }
    
    /// Release a reference. When ref count hits zero, enter pending_eviction state
    /// Don't delete immediately —— give a grace period (default 5 min) so short-term reuse still has a chance
    pub async fn release(
        &self,
        handle_id: &L3HandleId,
        session: &SessionId,
    ) -> Result<(), CacheError> {
        let record = self.by_id(handle_id)?;
        let mut sessions = record.referencing_sessions.write().unwrap();
        sessions.remove(session);
        
        if sessions.is_empty() {
            *record.pending_eviction_since.write().unwrap() = Some(SystemTime::now());
        }
        
        self.by_session.get_mut(session).map(|mut s| s.remove(handle_id));
        Ok(())
    }
    
    /// On full session termination, batch-release all handles held by that session
    pub async fn release_session(&self, session: &SessionId) -> Result<usize, CacheError> {
        let handles: Vec<_> = self.by_session.remove(session)
            .map(|(_, set)| set.into_iter().collect())
            .unwrap_or_default();
        
        for handle_id in &handles {
            self.release(handle_id, session).await.ok();
        }
        Ok(handles.len())
    }
    
    /// Mark a handle as used (for LRU sorting and metrics)
    pub fn mark_used(&self, handle_id: &L3HandleId) {
        if let Ok(record) = self.by_id(handle_id) {
            record.last_used_at.store(SystemTime::now());
            record.usage_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}
```

**Core invariants**:
1. The same `(CacheKey, ProviderId)` exists in the Registry **at most once** — guaranteed atomically by the `by_key` DashMap
2. Handle creation is singleflight — under concurrent acquires of the same unknown key, only one actually calls Provider create_cache; others wait for the result and reuse it
3. Session release is idempotent — repeated release of the same (handle, session) does not error
4. Hitting ref_count 0 enters pending_eviction; **does not delete immediately** — the grace period tolerates the common pattern of "just released, then a new request comes in"

### 7.3 Cross-Process Consistency

After process restart, the in-memory `by_key` / `by_session` indexes are cleared. But the Provider-side cache objects still exist (per their own TTL) — if the index can't be rebuilt after restart, **Provider-side resources leak (and continue costing money)**.

Solution:
1. **Persist to Postgres**: every register / extend / delete is sync'd to DB
2. **Reload on startup**: on process startup, read all unexpired handles from DB and rebuild in-memory indexes
3. **Periodic reconciliation**: hourly reconcile against the Provider (List API), delete entries the local has but the Provider has expired, alert on entries the Provider has but local lacks (potential leak)

```rust
async fn reconcile(&self) -> Result<ReconcileReport, CacheError> {
    let mut report = ReconcileReport::default();
    
    for provider_id in self.providers.list_explicit_cache_capable() {
        let provider = self.providers.get_explicit_cache(&provider_id)?;
        let remote = provider.list_caches().await?;        // Provider List API
        let local: HashSet<_> = self.by_key.iter()
            .flat_map(|entry| entry.value().iter()
                .filter(|r| r.provider == provider_id)
                .map(|r| r.external_id.clone()))
            .collect();
        
        // Remote has, local doesn't → leak (possibly created by another instance, or this instance crashed without cleanup)
        for remote_id in &remote {
            if !local.contains(remote_id) {
                report.orphans.push((provider_id.clone(), remote_id.clone()));
            }
        }
        
        // Local has, remote doesn't → already expired, clean up local index
        for record in self.records_for_provider(&provider_id) {
            if !remote.contains(&record.external_id) {
                self.remove_local_index(&record.id).await;
                report.cleaned_local += 1;
            }
        }
    }
    
    // Orphan handling: cross-instance scenarios may produce false positives → 30-min grace before deletion
    self.schedule_orphan_cleanup(report.orphans.clone(), Duration::from_mins(30));
    
    Ok(report)
}
```

---

## 8. Janitor (Active Reclamation)

The Janitor is a background task running four kinds of cleanup in a loop. **Core correction**: every L3 deletion must first check ref count; only handles with ref_count == 0 and past the grace period become eviction candidates. Session idleness alone cannot trigger deletion — it only triggers release; actual deletion depends on the global reference state.

```rust
pub struct CacheJanitor {
    registry: Arc<DefaultCacheRegistry>,
    providers: Arc<ProviderRegistry>,
    config: JanitorConfig,
}

pub struct JanitorConfig {
    pub tick_interval: Duration,                 // main loop interval, default 60s
    pub session_idle_timeout: Duration,          // session idle threshold, triggers release, default 15min
    pub eviction_grace_period: Duration,         // grace period after ref_count=0, default 5min
    pub l3_storage_quota_bytes: u64,             // total L3 storage quota
    pub l3_storage_high_water: f64,              // water mark that triggers LRU eviction (default 0.8)
    pub l3_storage_low_water: f64,               // water mark at which LRU eviction stops (default 0.6)
    pub reconcile_interval: Duration,            // reconcile with Provider, default 1h
}

impl CacheJanitor {
    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(self.config.tick_interval);
        let mut last_reconcile = Instant::now();
        
        loop {
            tick.tick().await;
            
            // 1. Idle sessions trigger release (does not necessarily delete handle immediately)
            self.release_idle_sessions().await;
            
            // 2. Handles with ref_count=0 past their grace period → actual deletion
            self.evict_unreferenced_handles().await;
            
            // 3. LRU eviction (when capacity exceeds water mark, may force-delete even if ref_count > 0)
            self.evict_lru_if_over_quota().await;
            
            // 4. Expiry cleanup (TTL elapsed)
            self.evict_expired().await;
            
            // 5. Reconcile (low frequency)
            if last_reconcile.elapsed() >= self.config.reconcile_interval {
                self.registry.reconcile().await.ok();
                last_reconcile = Instant::now();
            }
        }
    }
    
    /// Idle sessions trigger release — does not delete handles directly; only drops the session's references
    /// Actual deletion depends on whether other sessions still reference (ref counting)
    async fn release_idle_sessions(&self) {
        let cutoff = SystemTime::now() - self.config.session_idle_timeout;
        let stale_sessions: Vec<_> = self.registry.sessions_idle_since(cutoff).collect();
        
        for session_id in stale_sessions {
            let released = self.registry.release_session(&session_id).await.unwrap_or(0);
            self.metrics.record_session_released(&session_id, released);
        }
    }
    
    /// Actual deletion: ref_count == 0 and past the grace period
    async fn evict_unreferenced_handles(&self) {
        let candidates = self.registry.unreferenced_for(self.config.eviction_grace_period);
        for record in candidates {
            self.delete_l3_handle_force(&record.id).await.ok();
        }
    }
    
    /// LRU eviction: when capacity exceeds the water mark, force-delete even if ref_count > 0
    /// This is the last cost/capacity defense (LRU prefers low ref_count + earliest last_used)
    async fn evict_lru_if_over_quota(&self) {
        let total_size = self.registry.total_l3_size();
        let high = (self.config.l3_storage_quota_bytes as f64 * self.config.l3_storage_high_water) as u64;
        if total_size < high {
            return;
        }
        
        let target = (self.config.l3_storage_quota_bytes as f64 * self.config.l3_storage_low_water) as u64;
        let mut freed = 0u64;
        
        // LRU sorting: prefer ref_count == 0, then earliest last_used_at
        for record in self.registry.lru_iter_weighted() {
            if total_size - freed <= target { break; }
            
            // Force-evicting a ref > 0 handle has cost — referencers will cache miss and rebuild on next call
            if record.ref_count() > 0 {
                tracing::warn!(
                    handle = ?record.id,
                    ref_count = record.ref_count(),
                    "force evicting referenced handle due to quota pressure"
                );
                self.metrics.record_forced_eviction(&record);
                
                // Notify all referencing sessions: your handle is about to be invalidated
                for session in record.referencing_sessions.read().unwrap().iter() {
                    self.notify_handle_invalidation(session, &record.id);
                }
            }
            
            let size = record.size_estimate_bytes;
            if self.delete_l3_handle_force(&record.id).await.is_ok() {
                freed += size;
            }
        }
    }
    
    async fn delete_l3_handle_force(&self, handle_id: &L3HandleId) -> Result<(), CacheError> {
        let record = self.registry.get_handle(handle_id)
            .ok_or(CacheError::HandleNotFound)?;
        let provider = self.providers.get_explicit_cache(&record.provider)?;
        
        let provider_handle = ProviderCacheHandle {
            provider: record.provider.clone(),
            external_id: record.external_id.clone(),
            tenant_namespace: record.tenant.to_string(),
            created_at: record.created_at,
            expires_at: record.expires_at,
        };
        
        // If Provider delete fails, should we still remove from local index? No — that causes leaks
        // Strategy: mark as "pending_delete" and retry on next reconcile
        match provider.delete_cache(&provider_handle).await {
            Ok(_) => {
                self.registry.remove_handle(handle_id);
                self.metrics.record_eviction(&record);
                Ok(())
            }
            Err(e) if e.is_not_found() => {
                // Provider already deleted (TTL naturally elapsed); local cleanup suffices
                self.registry.remove_handle(handle_id);
                Ok(())
            }
            Err(e) => {
                tracing::warn!(?e, ?handle_id, "l3 delete failed, will retry");
                self.registry.mark_pending_delete(handle_id);
                Err(e.into())
            }
        }
    }
}
```

---

### 8.1 Weighted LRU Sorting (Eviction Priority)

Force-evicting a referenced handle is an expensive operation (the evicted session will miss on next acquire → rebuild), so LRU sorting cannot rely on time alone — it must be **weighted**:

```rust
fn eviction_priority(record: &L3HandleRecord) -> EvictionScore {
    let ref_count = record.ref_count() as i64;
    let idle_secs = record.last_used_at.elapsed().as_secs() as i64;
    let usage_count = record.usage_count.load(Ordering::Relaxed) as i64;
    
    // Larger = higher eviction priority
    EvictionScore(
        // ref_count = 0 has the highest priority (no side effects)
        if ref_count == 0 { 1_000_000 } else { 0 }
        // The longer idle, the higher priority
        + idle_secs
        // The fewer uses, the higher priority (avoid evicting hotspots)
        - usage_count.saturating_mul(10)
    )
}
```

---

## 9. Invalidation and Consistency

### 9.1 Explicit Invalidation Scenarios

- Upstream IAM changes (a user is removed from a project → that project's cache must invalidate immediately)
- Business data changes (the reviewed code repo has a new commit → the old review cache is stale)
- The user actively "regenerates"
- Compliance deletion (GDPR / tenant decommissioning → all caches for that tenant must be deleted)

### 9.2 Invalidation Granularity

```rust
#[async_trait]
pub trait CacheInvalidation {
    async fn invalidate_key(&self, key: &CacheKey) -> Result<()>;
    async fn invalidate_session(&self, session: &SessionId) -> Result<u64>;
    async fn invalidate_tenant(&self, tenant: &TenantId) -> Result<u64>;
    async fn invalidate_by_resource(&self, resource: &ResourceRef) -> Result<u64>;
}
```

### 9.3 Cross-Instance Invalidation

L1 is in-process; single-instance invalidation is insufficient — it must broadcast to all instances:

```rust
// Use Redis pub/sub to broadcast cross-instance invalidation
pub struct InvalidationBroadcaster {
    redis: Arc<RedisClient>,
    channel: String,                 // e.g. "tars:cache:invalidate"
    local_l1: Arc<L1Cache>,
}

impl InvalidationBroadcaster {
    pub async fn broadcast(&self, event: InvalidationEvent) -> Result<()> {
        // 1. Apply locally immediately
        self.apply_local(&event).await;
        
        // 2. Delete from Redis L2 immediately
        self.delete_from_l2(&event).await;
        
        // 3. Broadcast to other instances
        let payload = serde_json::to_string(&event)?;
        self.redis.publish(&self.channel, payload).await?;
        Ok(())
    }
    
    pub async fn run_subscriber(self: Arc<Self>) {
        let mut sub = self.redis.subscribe(&self.channel).await.unwrap();
        while let Some(msg) = sub.next().await {
            if let Ok(event) = serde_json::from_str::<InvalidationEvent>(&msg.payload) {
                self.apply_local(&event).await;
            }
        }
    }
}
```

### 9.4 Honesty About Eventual Consistency

Cross-instance L1 invalidation has a window (pub/sub latency typically <100ms, not guaranteed). This means:
- After the same cache key is invalidated, there may be a < 100ms window where some instance still returns the old response
- This is acceptable for an LLM cache — the cached content itself is a snapshot of a past LLM decision
- For **security-sensitive invalidations** (IAM changes), strong consistency is required: synchronously check the permission version at the IAM decision point; do not rely on cache invalidation broadcasts

---

## 10. Cross-Tenant Isolation Security Guarantees

This is the core security of the Cache Registry design, extracting the core requirements from the earlier discussion:

### 10.1 Three Lines of Defense

**Line 1: CacheKey namespace prefix** (§3.2)
TENANT + IAM SCOPES are the first bytes of the hash; even with identical prompts, tenant A and tenant B will inevitably have different keys.

**Line 2: IAM must precede Cache Lookup** (Doc 02 §4.2 hard constraint)
Even if CacheKey isolation is bypassed (say, due to a bug), IAM has already rejected unauthorized requests, so they never reach the Cache layer.

**Line 3: CacheKey reverse verification** (defense in depth)
On cache hit, perform a cheap ownership recheck:

```rust
async fn lookup(&self, key: &CacheKey, policy: &CachePolicy) -> Result<Option<CachedResponse>, CacheError> {
    let cached = match self.l1.get(key).await? {
        Some(c) => c,
        None => return Ok(None),
    };
    
    // Defensive check: cached's origin tenant must match the tenant encoded in current key
    // Theoretically impossible to mismatch (since key already includes tenant), but as final defense
    if !key.tenant_matches(&cached.cached_at_tenant) {
        tracing::error!(
            ?key.debug_label,
            cached_tenant = ?cached.cached_at_tenant,
            "CACHE TENANT MISMATCH - possible key collision or corruption"
        );
        self.metrics.record_security_alert("tenant_mismatch");
        return Ok(None);   // Treat as miss; trigger regeneration
    }
    
    Ok(Some(cached))
}
```

### 10.2 Provider-side Handle Isolation

L3 handles cannot be reused across tenants — the `tenant_namespace` field enforces this:

```rust
pub struct ProviderCacheHandle {
    pub provider: ProviderId,
    pub external_id: String,
    pub tenant_namespace: String,    // required; Provider adapters reject empty values
    // ...
}
```

When injecting an L3 handle into a request, CacheLookupMiddleware must validate:

```rust
fn validate_l3_use(handle: &ProviderCacheHandle, ctx: &RequestContext) -> Result<()> {
    if handle.tenant_namespace != ctx.tenant_id.to_string() {
        return Err(CacheError::TenantMismatch);
    }
    Ok(())
}
```

### 10.3 System Prompt Tenant Marker Injection

As mentioned earlier, Provider-side prefix caches hash by prompt bytes inside the Provider. Even if our Registry isolates cleanly, if two tenants' system prompts are byte-identical, the Provider's own implicit cache may hit the same entry — content won't leak, but billing attribution may be confused and timing side channels may emerge (an attacker infers via TTFT whether someone else asked similar questions).

**Mandatory rule**: all Provider adapters automatically inject the following at the head of the system prompt when constructing requests:

```
<!--
tenant_marker: {tenant_id_hash_first_8_chars}
session_marker: {session_id_hash_first_8_chars}
-->
```

The presence of this line makes prefixes byte-different across tenants, naturally isolating the Provider's implicit cache. This injection is performed at the Provider adapter layer, not the Cache layer — because this is the "isolation hole that bypasses the Provider's own cache", unrelated to our Cache Registry.

### 10.4 Cache Handles Must Never Leak

Doc 01 §10 already specifies: `ProviderCacheHandle.external_id` is a Provider-side "bearer pickup ticket" — any request holding the ID can use the cache. Therefore:

- Never return to frontend / client / logs (debug mode only outputs hash digests)
- API return values only expose our own `L3HandleId` (opaque UUIDs); when use is needed, the Registry translates to the real external_id

---

## 10.5 Cross-Session Sharing and Prompt Tiering (Design Guidance)

Cache Registry's Key construction (§3.2) already determines that **sharing boundary = Tenant ∩ IAM Scopes ∩ Model ∩ Static Content** — maximize sharing within boundaries, enforce independence outside. This means upper layers (Agent Runtime / prompt assembly) must structure prompts according to the following principles for caching to actually take effect:

### Three-Section Prompt Tiering (Naming Avoids L1/L2/L3 Conflict)

| Tier | Content | Change Frequency | Cache Strategy |
|---|---|---|---|
| **Static Prefix (cold)** | System Prompt, Agent role definition, JSON Schema, global tool spec | Monthly changes | Must be at the front; enters CacheKey; suitable for L3 explicit cache |
| **Project Anchor (warm)** | Codebase snapshot for the current project (bound to commit hash), API docs, dependency graph | Changes per commit | Follows Static Prefix; enters CacheKey; suitable for L3 |
| **Dynamic Suffix (hot)** | The user's current PR diff, real-time conversation, real-time logs | Changes every request | **Absolutely not in CacheKey**; appended on plain calls |

### Why This Tiering Enables Cross-Session Sharing

- Director A and intern B, in the same tenant, simultaneously review the same commit `a1b2c3` → Static Prefix + Project Anchor are byte-identical → CacheKey hash is identical → §7.2's acquire automatically returns the same L3 handle, ref count = 2
- Their reviewed PR diffs differ → Dynamic Suffix differs → LLM output differs → L1/L2 response cache won't hit across PRs (correct; it shouldn't)
- But L3 prefix cache hit rate is 100%; input tokens enjoy the discount

### Upper-Layer Responsibility

Cache Registry is not responsible for prompt assembly order — that's the Agent Runtime's job. Cache Registry only promises: **as long as your assembled Static portion is byte-identical (and other parameters match), I can share across sessions**.

This means the Agent Runtime must comply:
1. The assembly order of Static Prefix **must never change** outside config changes — adding a single space or tweaking a Markdown heading will miss the entire company's cache
2. Static Prefix must not contain dynamic variables (timestamps, request IDs, randomly generated instance markers) — these must move to Dynamic Suffix
3. Project Anchor must explicitly mark commit/version — `Codebase` is a dynamic concept, `Codebase@a1b2c3` is a static one
4. If tools schema doesn't change (typical within a project), it should be part of Static Prefix

The Agent Runtime's Prompt Builder should provide `static_prefix() -> String` and `dynamic_suffix() -> String` methods; Cache Lookup hashes only the former. This contract is detailed in Doc 04 (Agent Runtime).

---

## 11. Configuration Shape

```toml
[cache]
enabled = true
hasher_version = 1                      # bumping invalidates all L1/L2 at once

[cache.l1]
backend = "moka"
max_capacity = 10000
default_ttl_secs = 600
weighted_eviction = true                # weighted by response byte size

[cache.l2]
backend = "redis"
url = "redis://redis-cluster:6379"
default_ttl_secs = 86400
key_prefix = "tars:cache:"
compression = "zstd"                    # large response compression, saves 50-70% storage
serialization = "messagepack"

[cache.l3]
enabled = true
storage_quota_bytes = 10737418240       # 10 GB
high_water = 0.8
low_water = 0.6
default_ttl_secs = 3600

[cache.singleflight]
enabled = true
auto_threshold_inflight = 5             # auto-enable when in-flight > 5
follower_timeout_secs = 120

[cache.janitor]
tick_interval_secs = 60
session_idle_timeout_secs = 900
reconcile_interval_secs = 3600

[cache.invalidation]
broadcast_channel = "tars:cache:invalidate"

# Tenant-level overrides
[tenants.acme_corp.cache]
l3_storage_quota_bytes = 53687091200    # 50 GB; larger quota for big customers

# Security knobs (rarely touched in production)
[cache.security]
strict_tenant_check = true              # enables §10.1 line of defense 3
require_iam_scopes = true               # missing iam.allowed_scopes errors out (otherwise falls back to empty)
```

---

## 12. Metrics and Observability

Required metrics to collect:

| Metric | Type | Labels | Purpose |
|---|---|---|---|
| `cache.lookup.total` | counter | level, tenant, hit/miss | Hit rate tracking |
| `cache.lookup.latency` | histogram | level | L1/L2/L3 performance monitoring |
| `cache.write.total` | counter | level, tenant | Write volume |
| `cache.write.errors` | counter | level, error_kind | L2 failure alerts |
| `cache.evictions` | counter | reason (idle/lru/expired/explicit) | Capacity tuning |
| `cache.l3.handles` | gauge | provider, tenant | L3 count monitoring |
| `cache.l3.storage_bytes` | gauge | provider, tenant | Capacity water mark |
| `cache.l3.cost_saved_usd` | counter | provider, tenant | ROI proof |
| `cache.singleflight.coalesced` | counter | tenant | Stampede protection effectiveness |
| `cache.security.alerts` | counter | kind | Anomaly alerts (tenant_mismatch, etc.) |
| `cache.invalidation.broadcast` | counter | scope | Invalidation events |

**Cost savings accounting formula**:

```
cost_saved = sum(cached_response.original_usage priced under current model) - 0
```

Accumulated on every L1/L2 hit, reflecting "how much it would have cost without the hit". This is the key metric for convincing management to invest in cache infrastructure.

---

## 13. Anti-Pattern Checklist

1. **Don't use prompt as the entire cache key input** — must include TENANT + IAM_SCOPES + MODEL + all output-determining parameters.
2. **Don't expose ProviderCacheHandle.external_id to frontend / API callers** — it's a bearer credential; leakage = cross-tenant access vulnerability.
3. **Don't skip IAM checks on cache hit** — IAM must occur before Lookup (Doc 02 §4.2); this is a security defense.
4. **Don't synchronously block on the write path** — L2 writes must spawn async tasks and not block response return.
5. **Don't cache incomplete responses** — MaxTokens / Cancelled / ContentFilter do not enter cache.
6. **Don't let cache errors contaminate business** — when Redis is down, degrade to miss; never block LLM calls.
7. **Don't change the hash algorithm without bumping hasher_version** — algorithm upgrades must bump version to invalidate old caches at once.
8. **Don't trust Provider TTL auto-expiry** — active delete is true cost control (§8 Janitor).
9. **Don't let the Janitor halt because a single delete failed** — mark failed deletions pending_delete and retry next time.
10. **Don't assume JSON serialization is deterministic** — different libs / different runtimes may emit different bytes; use RFC 8785-style canonical JSON.
11. **Don't include trace_id / timestamp / request_id in the cache key** — these change every time, guaranteeing 0% hit rate.
12. **Don't let singleflight followers wait beyond a reasonable time** — when the leader dies, followers fall back themselves; never wait indefinitely.
13. **Don't mix caches across hasher_versions** — same key bytes but different semantics; you'll read wrong data.
14. **Don't maintain "business object → cache key" mappings inside the Cache Registry** — business object invalidation should trigger via ResourceRef and invalidate_by_resource, not have the Cache layer reverse-track business dependencies.

---

## 14. Contracts with Upstream and Downstream

### Upstream (CacheLookupMiddleware) Promises

- Before calling lookup, IAM has already decided; `ctx.attributes["iam.allowed_scopes"]` is populated
- When calling write, the response is complete (already filtered by `should_cache()`)
- The `cache_directives` field of ChatRequest is populated by this layer; no other fields are modified

### Downstream (ExplicitCacheProvider) Promises

- The handle returned by `create_cache` is immediately usable
- `delete_cache` is idempotent (multiple deletes of the same handle return NotFound rather than error)
- `list_caches` is used for Janitor reconcile and returns all unexpired cache external_ids under that account

### Cross-Instance Contract

- All instances share L2 (Redis)
- L1 invalidation is broadcast via Redis pub/sub
- The truth source for L3 handle registry is Postgres; each instance's in-memory index is a read-only replica, reloaded on restart

---

## 15. TODOs and Open Questions

- [ ] Decide on the canonical JSON implementation: in-house vs `cjson-rs` vs `jcs` (RFC 8785)
- [ ] L2 Redis HA scheme: Sentinel vs Cluster
- [ ] Multi-Provider dimensional modeling for L3 storage_quota (Gemini quota and Anthropic quota are independent)
- [ ] Cross-DC L2 deployment strategy (per-DC independent Redis vs global Redis vs CRDT)
- [ ] Degradation when Provider List API is absent for L3 reconcile (Gemini provides List; Anthropic ephemeral cache does not)
- [ ] Cache pre-warming: is it worth pre-creating L3 handles for high-frequency prompts
- [ ] Statistical precision of the cache_saved_usd metric (use original model's current pricing on hit vs pricing at hit time)
