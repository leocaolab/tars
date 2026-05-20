# Cost & reliability stack — recipes

How to actually use the four middlewares shipped in roadmap §1–§4:

| Middleware | Stateless? | Solves |
|---|---|---|
| `RetryMiddleware` (with `max_wait`) | yes | Transient flake on one provider |
| `PerCallBudgetMiddleware` | yes | "No single call can cost more than $X" |
| `TenantBudgetMiddleware` | stateful (via `BudgetStore`) | "Tenant Y has $Z left this month" |
| `FallbackMiddleware` | yes | Provider dies / over-budget → try another |

The design rationale is in
[`docs/roadmap.md`](../roadmap.md). This doc is the **how to use it**
cookbook — copy-paste recipes by complexity, then gotchas.

---

## Recipe 1 — per-call hard cap (smallest useful setup)

The minimum stack that solves Cando-Peter's `<$0.05/draft` requirement.
No tenant tracking; each call is checked independently.

```rust
use std::sync::Arc;
use tars_pipeline::{
    Pipeline, PerCallBudgetMiddleware, RetryMiddleware, TelemetryMiddleware,
};

let provider = registry.get(&ProviderId::new("anthropic")).unwrap();
let caps = provider.capabilities();

let pipeline = Pipeline::builder(provider)
    .layer(TelemetryMiddleware::new())
    .layer(PerCallBudgetMiddleware::new(0.05, caps))   // <$0.05/call
    .layer(RetryMiddleware::default())
    .build();

match Arc::new(pipeline).call(req, ctx).await {
    Ok(stream) => { /* consume */ }
    Err(ProviderError::BudgetExceeded) => {
        // Pre-call estimate exceeded the cap; nothing was sent.
    }
    Err(other) => { /* normal error path */ }
}
```

**What the user sees**: if `(chars/4 × input_pricing + max_output × output_pricing)`
exceeds `0.05`, the call returns `ProviderError::BudgetExceeded`
**before** any network round-trip. The pricing comes from the provider's
`Capabilities` (Anthropic's official pricing for that model).

**Subscription backends** (`claude_cli`, `gemini_cli`, `codex_cli`) have
`Pricing::default()` (all zeros). The middleware detects this, logs
one `tracing::warn` per service, and passes through. No surprises.

---

## Recipe 2 — per-call cap + tenant cap

Same as Recipe 1, plus aggregate per-tenant tracking. Standard
multi-tenant production setup.

```rust
use std::sync::Arc;
use tars_pipeline::{
    Pipeline, PerCallBudgetMiddleware, TenantBudgetMiddleware,
    InMemoryBudgetStore, RetryMiddleware, TelemetryMiddleware,
};
use tars_types::TenantId;

let store = Arc::new(InMemoryBudgetStore::new());
// Configure tenants explicitly. Unconfigured tenants are unlimited
// (so adding the middleware doesn't break existing flows).
store.set(&TenantId::new("acme"),       100.00).await;  // $100 cap
store.set(&TenantId::new("dev-team"),    10.00).await;  // $10 cap
// "ghost" tenant has no entry → treated as unlimited.

let pipeline = Pipeline::builder(provider)
    .layer(TelemetryMiddleware::new())
    .layer(PerCallBudgetMiddleware::new(0.05, caps))
    .layer(TenantBudgetMiddleware::new(store.clone(), caps))
    .layer(RetryMiddleware::default())
    .build();
```

**How they compose**: the per-call cap fires first (cheap, no I/O). If
the estimate fits, the tenant cap fires next (I/O against the store).
On success, the tenant cap **debits the real usage from the stream's
`Finished` event**, not the upper-bound estimate — so tenants pay for
what they actually used.

**Reading the remaining balance**:

```rust
let remaining = store.remaining(&TenantId::new("acme")).await?;
match remaining {
    Some(usd) => println!("acme has ${usd:.4} left"),
    None      => println!("acme is unconfigured (unlimited)"),
}
```

---

## Recipe 3 — full production stack with fallback

What you actually want in production: cost control + capacity
fallback + retry. This is the stack Cando-Peter would adopt today.

```rust
use std::sync::Arc;
use tars_pipeline::{
    Pipeline, PerCallBudgetMiddleware, TenantBudgetMiddleware,
    InMemoryBudgetStore, FallbackMiddleware, FallbackTrigger,
    RetryMiddleware, TelemetryMiddleware, CacheLookupMiddleware,
};

// Construct three providers in priority order.
let opus   = registry.get(&ProviderId::new("anthropic_opus")).unwrap();
let sonnet = registry.get(&ProviderId::new("anthropic_sonnet")).unwrap();
let local  = registry.get(&ProviderId::new("vllm_local")).unwrap();

let store = Arc::new(InMemoryBudgetStore::new());
store.set(&TenantId::new("acme"), 100.00).await;

let pipeline = Pipeline::builder(opus.clone())                       // primary
    .layer(TelemetryMiddleware::new())                                // outermost
    .layer(CacheLookupMiddleware::new(cache))                         // free on hit
    .layer(PerCallBudgetMiddleware::new(0.05, opus.capabilities()))   // hard cap
    .layer(TenantBudgetMiddleware::new(store.clone(),                 // tenant cap
                                       opus.capabilities()))
    .layer(FallbackMiddleware::builder()                              // typed degrade
        .fallback_to_provider(sonnet.clone(), FallbackTrigger::cost_related())
        .fallback_to_provider(local.clone(),  FallbackTrigger::availability())
        .build())
    .layer(RetryMiddleware::default())                                // single-provider flakes
    .build();
```

### What this stack does on each scenario

| Scenario | Behavior |
|---|---|
| Normal call | Telemetry → Cache miss → Budget OK → Tenant OK → Retry → Opus → response |
| Cache hit | Telemetry → Cache hit → response (everything below cache is skipped, **including budget checks** — cache hits are free) |
| Opus over per-call budget | `PerCallBudget` rejects with `BudgetExceeded` → `FallbackMiddleware` catches → tries Sonnet (cheaper pricing → fits) → response |
| Opus 429 with short Retry-After | Retry sleeps and retries on Opus → success |
| Opus 429 with 30+ min Retry-After | `RetryMiddleware` bubbles past `max_wait` → Fallback catches → tries Sonnet → if also 429, tries vllm_local → response |
| Opus + Sonnet both rate-limited, vllm_local healthy | Fallback walks the chain → succeeds on vllm_local |
| `tenant=acme` has $0.02 left, $0.04 call | `TenantBudget` rejects with `BudgetExceeded` → Fallback → Sonnet cap might also reject (or pass if pricing is lower) → eventually fails or succeeds on cheap-enough hop |
| Invalid request (400) | `Permanent` error class — Fallback does **not** trigger; bubbles immediately |

### Layer ordering rationale (read top-to-bottom = outside-in)

```
Telemetry          ← sees everything, even cache hits
  Cache            ← short-circuits cheaply
    PerCallBudget  ← stateless USD upper-bound check
      TenantBudget ← stateful per-tenant debit
        Fallback   ← typed errors below this point → switch provider
          Retry    ← same-provider short flakes; bubbles long waits past Fallback
            Provider call
```

The two important non-obvious orderings:

1. **Cache outside Budget.** Cache hits are free — no point pre-checking budget for them.
2. **Fallback outside Retry.** Each fallback hop gets its own full retry budget on its own provider. The reverse would burn fallback slots on a single 429.

---

## Recipe 4 — plug in your own BudgetStore

The shipped `InMemoryBudgetStore` is fine for dev, tests, and
single-process deploys. Multi-process production needs a shared
backend.

```rust
use async_trait::async_trait;
use tars_pipeline::{BudgetStore, BudgetStoreError};
use tars_types::TenantId;

struct RedisBudgetStore { conn: redis::aio::ConnectionManager }

#[async_trait]
impl BudgetStore for RedisBudgetStore {
    async fn remaining(&self, t: &TenantId) -> Result<Option<f64>, BudgetStoreError> {
        let key = format!("budget:{}", t.as_str());
        match self.conn.clone().get::<_, Option<String>>(key).await {
            Ok(Some(s)) => s.parse().map(Some).map_err(|e| {
                BudgetStoreError::Backend(format!("parse: {e}"))
            }),
            Ok(None)    => Ok(None),
            Err(e)      => Err(BudgetStoreError::Backend(e.to_string())),
        }
    }

    async fn debit(&self, t: &TenantId, amount: f64) -> Result<Option<f64>, BudgetStoreError> {
        // Use Redis WATCH + MULTI for atomic check-and-decrement
        // if you want strict (not soft-cap) accounting. Skipped here
        // for brevity; the trait contract allows brief negative
        // balances under concurrent debits.
        unimplemented!()
    }
}

let store: Arc<dyn BudgetStore> = Arc::new(RedisBudgetStore { conn });
let mw = TenantBudgetMiddleware::new(store, caps);
```

**Contract** (from the trait doc):

- `remaining(tenant)` returning `None` = unconfigured = unlimited (don't reject).
- `debit(tenant, amount)` returning `None` = same — the tenant became unconfigured between pre-check and debit. Log and continue.
- Either method returning `Err(BudgetStoreError::Backend(_))`:
  - On `remaining`: middleware **fails closed** with `ProviderError::Internal`. A store outage cannot silently uncap a tenant.
  - On `debit`: middleware logs at `error` level but **does not** fail the call (the user already got their response). Operators investigate.

---

## Gotchas

### 1. Estimation is a strict upper bound

Both budget middlewares estimate cost as:

```
input_tokens  ≈ (system_chars + sum(message_text_chars)) / 4
output_tokens =  req.max_output_tokens ?? capabilities.max_output_tokens
cost          =  input_tokens × pricing.input_per_million  / 1e6
              +  output_tokens × pricing.output_per_million / 1e6
```

No cached-input or cache-creation discounts are subtracted — pre-call
we don't know cache state. The estimate is the **worst case**; real
post-call debits subtract via `Pricing::cost_for(&usage)` from the
actual `usage` (which does account for cache).

**Implication**: tenants are pre-checked against worst case, debited
real cost. A tenant with `$0.05` left and a `$0.04` real call may be
rejected if the pre-check estimate is `$0.06`. Set caps with headroom.

### 2. `max_output_tokens = None` falls back to capability default

If `req.max_output_tokens` is `None`, the worst-case bound becomes
`Capabilities.max_output_tokens` (8192 for most Sonnet/Opus). This
makes "open-ended" requests look expensive. **Set `max_output_tokens`
explicitly** on cost-sensitive call paths — it's the single biggest
knob you control.

### 3. Subscription backends bypass budgets

`claude_cli` / `gemini_cli` / `codex_cli` have `Pricing::default()`
(zeros). Both budget middlewares detect this, warn once, and pass
through. **There is no way to cap subscription calls in USD** —
that's not what the subscription billing model exposes. If you need
caps, use the HTTP API path.

### 4. Fallback triggers are keyed on error **kind**, not class

`ErrorClass` is too coarse: `BudgetExceeded` and `ContextTooLong` are
both `Permanent`, but want different fallback strategies. So
`FallbackTrigger::on(&["budget_exceeded"])` uses
`ProviderError::kind()` strings — see the
[full list of kinds](../../crates/tars-types/src/error.rs).

Shipped canned triggers:

```rust
FallbackTrigger::cost_related()   // budget_exceeded + context_too_long
FallbackTrigger::availability()   // rate_limited + model_overloaded + circuit_open + network
FallbackTrigger::any()            // last-resort kitchen sink (use sparingly)
```

### 5. Don't use `FallbackTrigger::any()` on hop 1

`any()` is for the **last** hop only — it would mask real bugs
(`invalid_request` on hop 1 would silently retry on hop 2 with the
same broken input). Keep early hops tied to specific recoverable
kinds.

### 6. `max_wait` cooperates with `Fallback`, not against it

`RetryMiddleware`'s `max_wait` (default 30 s) is the **bridge** to
Fallback. When a provider says "wait 1 hour," Retry bubbles past
its cap → outer Fallback catches and switches provider. Without
Fallback, the error reaches the caller — exactly what you want for
"don't sleep 30 minutes inside one call."

### 7. Pricing is per-provider — pass the right capabilities

```rust
// WRONG: using primary's caps for everyone
PerCallBudgetMiddleware::new(0.05, opus_caps)
// applied to Sonnet via fallback → estimates Opus pricing → too strict

// RIGHT: each provider gets its own middleware in its own pipeline
let sonnet_svc = Pipeline::builder(sonnet.clone())
    .layer(PerCallBudgetMiddleware::new(0.05, sonnet.capabilities()))
    .layer(RetryMiddleware::default())
    .build();
let mw = FallbackMiddleware::builder()
    .fallback_to_service(Arc::new(sonnet_svc), FallbackTrigger::cost_related())
    .build();
```

The `fallback_to_service` form (vs `fallback_to_provider`) lets each
hop have its own properly-priced budget middleware.

---

## Observability

Every layer emits `tracing` events. Pipe stderr to JSON for log
aggregators:

```bash
tars run --log-format json ... 2>tars.jsonl
```

Then:

```bash
# Budget rejections in the last hour
jq -c 'select(.message | test("budget.*exceeded"))' tars.jsonl

# Fallback hops fired and why
jq -c 'select(.message=="fallback: primary failed, switching to next hop")' tars.jsonl

# Retry exhaustions
jq -c 'select(.message | test("retry.*exhausted|retry.*bubbling"))' tars.jsonl
```

Persistent events (one row per LLM call, with usage and latency) live
in `~/.tars/events/pipeline_events.db` and are queryable via
`tars events list / show`. See
[`observability.md`](../observability.md) for the full guide.

---

## See also

- [`../roadmap.md`](../roadmap.md) — design rationale and what's NOT yet shipped (§5 batch mode)
- [`../USER-GUIDE.md`](../USER-GUIDE.md) — the calling shapes that these middlewares sit on top of
- [`../observability.md`](../observability.md) — how to watch what your stack is actually doing
- [`../providers/claude-cli.md`](../providers/claude-cli.md) — subscription backend caveat (no USD caps)
