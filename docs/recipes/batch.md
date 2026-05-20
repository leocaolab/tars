# Batch mode — recipes

Batch APIs let you submit many LLM calls for offline processing at
**~50% of sync pricing** with up to a 24 h SLA. Three vendors with
three different shapes — tars unifies them behind one trait so
caller code is identical.

| Provider | Status | Submit shape | Output |
|---|---|---|---|
| **Anthropic** | ✅ Supported | One-step inline JSON `requests[]` | GET results endpoint (JSONL) |
| **OpenAI** | ✅ Supported | Two-step: upload JSONL file, create batch referencing file id | GET `output_file_id` content (JSONL) |
| **Gemini** | ⛔ Surface present, not implemented | Long-Running Operations + Vertex AI path | See [§Gemini](#gemini) |

If you need batch and your code runs against Anthropic or OpenAI, you
can start shipping today. If you need Gemini batch, see the deferral
note below.

---

## 1. The smallest possible recipe

```rust
use std::sync::Arc;
use tars_provider::ProviderRegistry;
use tars_types::{BatchItemId, ChatRequest, ModelHint, ProviderId};

let provider = registry.get(&ProviderId::new("anthropic")).unwrap();
let submitter = provider
    .as_batch_submitter()
    .expect("anthropic supports batch");

// 1) Submit.
let job_id = submitter
    .submit(vec![
        (
            BatchItemId::new("draft-1"),
            ChatRequest::user(ModelHint::Explicit("claude-opus-4-7".into()), "first prompt"),
        ),
        (
            BatchItemId::new("draft-2"),
            ChatRequest::user(ModelHint::Explicit("claude-opus-4-7".into()), "second prompt"),
        ),
    ])
    .await?;

// 2) Persist the job_id somewhere — caller is responsible for this.
my_db.save_batch_job(&job_id);

// 3) Later (or in another process): poll status.
loop {
    use tars_types::BatchStatus;
    match submitter.status(&job_id).await? {
        BatchStatus::Completed => break,
        BatchStatus::Failed { kind, message } => {
            return Err(format!("batch failed: {kind}: {message}").into());
        }
        BatchStatus::Expired => return Err("batch expired".into()),
        BatchStatus::Cancelled => return Err("batch cancelled".into()),
        _ => tokio::time::sleep(std::time::Duration::from_secs(60)).await,
    }
}

// 4) Fetch results.
for item in submitter.results(&job_id).await? {
    match item.result {
        Ok(resp) => println!("{}: {}", item.item_id, resp.text),
        Err(e)   => eprintln!("{} failed: {e}", item.item_id),
    }
}
```

**Key points** (these surface differently from sync calls — read carefully):

- `as_batch_submitter()` returns `Option<Arc<dyn BatchSubmitter>>` —
  not every provider implements it (`claude_cli` / `gemini_cli` / etc.
  don't). Pattern-match before using.
- `BatchItemId` is **caller-chosen**. The vendor echoes it back in
  results so you can correlate outputs to inputs regardless of order.
- `BatchJobId` is **vendor-issued**. Persist it — tars stores nothing.
- **Per-item failures don't fail the job.** A batch with one bad item
  out of 10k still ends `Completed`; check `item.result` per item.

---

## 2. Polling: when and how often

| Vendor | Typical completion | Recommended poll interval |
|---|---|---|
| Anthropic | seconds to hours | 60 s for small batches, 5 min for >10k items |
| OpenAI | minutes to hours | 60 s |
| Both | hard SLA 24 h | If still in-flight at 23 h, prepare to retry |

Don't poll faster than 30 s — vendors throttle status requests and
rapid polling burns your sync-API rate limit budget for no benefit.

`BatchStatus::is_terminal()` is the right predicate for "stop polling":

```rust
loop {
    let st = submitter.status(&job_id).await?;
    if st.is_terminal() {
        break st;
    }
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
};
```

---

## 3. Status mapping reference

`BatchStatus` is vendor-neutral. The vendor-specific status vocab maps
in (handled inside tars):

```text
BatchStatus::Submitted        — accepted, not yet processing
BatchStatus::InProgress       — processed/total counts when reported
{ processed, total, eta }
BatchStatus::Completed        — terminal; per-item results in results()
BatchStatus::Failed           — terminal; job-level failure
{ kind, message }                (auth, input validation, etc.)
BatchStatus::Expired          — terminal; 24h SLA hit
BatchStatus::Cancelled        — terminal; caller cancel()'d
```

Vendor source mapping:

| Vendor field | → tars BatchStatus |
|---|---|
| Anthropic `processing_status="in_progress"` | InProgress |
| Anthropic `processing_status="canceling"` | InProgress |
| Anthropic `processing_status="ended"` + all `canceled` | Cancelled |
| Anthropic `processing_status="ended"` + all `expired` | Expired |
| Anthropic `processing_status="ended"` (other) | Completed |
| OpenAI `status="validating\|in_progress\|finalizing\|cancelling"` | InProgress |
| OpenAI `status="completed"` | Completed |
| OpenAI `status="failed"` | Failed (message from `errors.data[0]`) |
| OpenAI `status="expired"` | Expired |
| OpenAI `status="cancelled"` | Cancelled |

---

## 4. Cost: what you're actually saving

The vendor batch endpoint applies its discount automatically (no flag
to pass). The pricing in `Capabilities` is for sync — for batch you're
paying ~half. tars does not have a separate "batch pricing" field
today, so if you use `TenantBudgetMiddleware` to track aggregate spend
and you mix sync + batch, your accounting will over-charge batch calls
relative to actual invoices. Two options:

- **Track batch separately.** Don't put batch calls through the same
  budget middleware as sync.
- **Adjust post-hoc.** Use the real `usage` from `BatchResultItem`
  responses + multiply by `0.5 × Capabilities.pricing.cost_for(...)`
  for the actual batch invoice number.

A "BatchPricing" field on Capabilities is a known V2 ask but not in
scope yet.

---

## 5. Cancel

```rust
submitter.cancel(&job_id).await?;
```

Vendors that support it:
- **Anthropic** — yes, transitions to `canceling` then `cancelled`.
- **OpenAI** — yes, transitions to `cancelling` then `cancelled`.
- **Gemini** — surface present, returns `InvalidRequest`.

Per-item cancel (within a still-running batch) is not supported by
any vendor.

---

## 6. Error handling

### Job-level vs item-level

```text
                  ┌─ submit() → Err(_)            : job never started
                  │
job lifecycle ─┼─ status() → Failed/Expired      : job aborted before completion
                  │
                  └─ status() → Completed
                                 └─ results()[i].result → Err(_)  : one item failed
```

Common item-level failures and their typed mapping:

| Vendor error code | → tars ProviderError |
|---|---|
| Anthropic `invalid_request_error` | InvalidRequest |
| Anthropic `authentication_error` | Auth |
| Anthropic `rate_limit_error` | RateLimited |
| Anthropic `overloaded_error` | ModelOverloaded |
| OpenAI `invalid_request` / `invalid_request_error` | InvalidRequest |
| OpenAI `rate_limit_exceeded` | RateLimited |
| (other) | Internal (preserving vendor code in the message) |

The "everything else" bucket is intentional — vendors add error codes
over time. Internal preserves the original code in the message so it's
still grep-able while keeping our error enum stable.

### `results()` on non-terminal jobs

`results()` calls `status()` first and returns `InvalidRequest` if the
job isn't terminal yet. This is uniform across vendors — Anthropic's
results endpoint 404s on incomplete jobs, OpenAI doesn't have an
output file yet, but our trait surface looks the same either way.

Caller must poll status first and only call `results()` once a
terminal state is reached.

---

## 7. What tars does NOT do for batch

Per the [agent-runtime scope discipline](../roadmap.md#scope-discipline-applies-to-everything-below):

| Concern | Who |
|---|---|
| **Scheduling** ("poll every minute") | Your code. tars provides `status()`; you call it. |
| **Persistence of BatchJobId** | Your DB. tars treats IDs as opaque strings. |
| **Mixing batch + sync in one logical call** | Caller picks the path. Two APIs, two access patterns. |
| **Auto-retry of failed batches** | Caller decides. Per-item retry policy is app-specific. |
| **Batch result event-store integration** | V2 — `BatchResultItem` is currently in-memory only. |

These are explicit non-goals, not "we forgot." See the
[`BatchSubmitter` trait module-level docs](../../crates/tars-provider/src/batch.rs)
for the rationale.

---

## 8. Known gaps

### Tool calls in batch responses

Both Anthropic and OpenAI's batch result parsers in tars **skip
`tool_use` / `tool_calls`** content blocks. V1 ships text-content-only.
Batch use cases are mostly draft generation, which doesn't need tool
calls; agentic batch is rare. If you need tool calls in batch:

- Workaround: parse the raw vendor response JSON yourself via
  `BatchResultItem.result` (currently exposes only `ChatResponse`,
  so this is also blocked — see V2).
- V2 will replay `ChatEvent::ToolCallStart / Args / End` through the
  builder for batch results.

### Image / vision in batch inputs

Image content blocks pass through serialization unchanged — they
should work, but no specific batch+vision tests exist. File an issue
if you find divergence.

### Gemini

Returns `ProviderError::InvalidRequest("Gemini batch is not yet
implemented...")` from all four methods. Both feasible Gemini batch
paths require work tars doesn't ship today:

- **GenAI API batch** uses Long-Running Operations (LRO), a different
  polling protocol from sync; needs a separate code path.
- **Vertex AI Batch Prediction** uses service-account auth + a GCS
  bucket for input/output, which is **explicitly out of scope** for
  this backend (the GeminiProvider uses API-key auth on the GenAI
  endpoint).

Tracked in [roadmap §5 Phase 4](../roadmap.md). Re-opening this is a
contributor-welcome ticket — needs someone to pin the current GenAI
batch API shape and add LRO polling support.

---

## See also

- [`../roadmap.md`](../roadmap.md) — design rationale + scope discipline
- [`cost-and-reliability.md`](./cost-and-reliability.md) — the four
  middlewares that apply to *sync* calls (not batch — see §4 above)
- [`../providers/anthropic.md`](../providers/anthropic.md), [`../providers/claude-cli.md`](../providers/claude-cli.md)
