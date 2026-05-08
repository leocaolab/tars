# Doc 15 — Output Validation (Output Validation Middleware)

> Scope: After the LLM returns a Response and before it is handed back to the caller, run a set of registered `OutputValidator`s to check / rewrite / reject the output. Move "validating the output contract" from being reimplemented per consumer → into a built-in middleware in tars-pipeline.
>
> Upstream: Doc 02 Middleware Pipeline (the ValidationMiddleware introduced here is one of its layers); Doc 01 LLM Provider (`Response` / `ChatRequest` types).
>
> Downstream: every consumer that needs "output conforms to a contract". The first user is the critic agent of a downstream consumer — migrating the `_known_rule_ids` whitelist logic currently inlined in `app/core/critic_agent.py`.
>
> **What this doc explicitly does NOT cover**: this doc discusses **synchronous, single-call, Response-affecting contract validation**. Multi-dimensional scoring / time series / dataset comparison / offline release gates are **evaluation**, not **validation** — see Doc 16. The two concepts must not be mixed into the same trait.

---

## 1. Design goals

| Goal | Description |
|---|---|
| **Single shared implementation** | "Output contract" code like rule_id whitelist / JSON shape check / length check should not be rewritten by every consumer |
| **Explicit Pass / Filter / Reject / Annotate disposition** | Each outcome has clear semantics; when writing a validator you don't have to wonder "should this transform or throw" |
| **Reuse existing retry path** | Reject goes through `ProviderError::ValidationFailed`, the existing `RetryMiddleware` decides whether to retry; no second retry mechanism is introduced |
| **Plugin-style registration** | Caller adds with one line: `Pipeline::builder().layer(ValidationMiddleware::new(vec![...]))`; a few built-ins cover 80% of cases, custom ones implement the trait |
| **Cross-language** | Python implements via the `tars.OutputValidator` base class; future Node mirrors the shape (same pattern as Stage 3 PyTool) |
| **Order is explicit** | `add_validator` registration order = execution order, left to right; Filter chains (each one sees the Response after the previous Filter) |
| **Streaming not blocked** | Validators always operate on the complete Response (called after draining the stream), but the token-by-token streaming UX on the caller side can still be preserved — the validator just runs once synchronously when the stream ends |

**Anti-goals**:

- **Not a replacement for schema constraints**: the `response_schema` kwarg (Doc 12) remains the first choice — it takes effect at provider decode time. Validation is the fallback when schema doesn't apply / isn't available. The two coexist, not mutually exclusive.
- **Not evaluation**: a validator only produces an outcome, not a dimension score; doesn't write time series; doesn't do offline dataset comparisons. Those belong to Doc 16.
- **Don't run expensive LLM-as-judge inside a validator**: blocking per call, hurts latency. If you really want LLM-judge, build it as a Doc 16 async evaluator with results fed back via EventStore, non-blocking on the response.

---

## 2. Architecture overview

ValidationMiddleware sits in the [Doc 02 §2 onion diagram](./02-middleware-pipeline.md) at: **inside Retry, outside Provider**.

```
   ... → Retry → ValidationMiddleware → Provider
                       │
                       │ (drain stream into Response)
                       ▼
                  ┌─────────────────────────────────┐
                  │  validators[0].validate(resp)   │
                  │  ↓ Pass | Filter | Reject | Annotate
                  │  validators[1].validate(resp')  │
                  │  ↓ ...                          │
                  └─────────────────────────────────┘
                       │
                       │ Pass        → flow back unchanged
                       │ Filter      → resp replaced with transformed, flow back transformed
                       │ Reject      → Err(ProviderError::ValidationFailed{retriable})
                       │              ↑ Retry sees this and decides whether to retry
                       │ Annotate    → flow back unchanged, but write metrics to RequestContext.attributes
                       ▼
                  Response received by caller
```

**Why inside Retry, outside Provider**:

- **Outside Retry** (inside Provider) means a validation failure cannot trigger retry — the validator rejected and we want to call the model again, but Pipeline has already returned, too late
- **Inside Retry** (the layer inside Provider), ValidationMiddleware throws `ValidationFailed { retriable: true }`, the outer RetryMiddleware sees `ErrorClass::Retriable` and naturally retries, **reusing the existing retry infrastructure with zero extra coupling**
- **Inside Provider** (further in) means ValidationMiddleware can't see the Response, because Provider directly produces the stream

**Why we must drain the entire stream**:

Most meaningful checks a validator does (rule_id whitelist, JSON shape, finding count, tag completeness) require the complete Response. They cannot be judged mid token-stream. **Validator is a post-stream concept.**

Cost: the stream Pipeline gives the caller still feels streaming (caller writes `for chunk in stream: print(chunk)` unchanged), but each chunk is internally drained to completion in ValidationMiddleware before being re-emitted. The "stream" the caller sees is a replay, not real token-by-token — latency is equivalent to "complete generation + single emit". Zero impact on **non-interactive review / classification / background batch** scenarios; impact on **face-to-face typing chatbot UX** (first token latency delayed until generation completes). The latter shouldn't enable ValidationMiddleware, or should enable it but only run non-blocking Annotate validators.

---

## 3. Core types

### 3.1 `OutputValidator` trait

```rust
// Location: tars-pipeline::validation
pub trait OutputValidator: Send + Sync {
    /// Stable name used for telemetry, logs, and ordering hints.
    fn name(&self) -> &str;

    /// Run the validator against a (req, resp) pair. The request is
    /// included so validators that need original prompt context
    /// (e.g. SnippetGroundingValidator wants the source file) can use
    /// it; most validators ignore it.
    fn validate(&self, req: &ChatRequest, resp: &Response) -> ValidationOutcome;
}

pub enum ValidationOutcome {
    /// Response unchanged, no metrics recorded.
    Pass,

    /// Response transformed in-place. The new Response is what
    /// downstream sees. `dropped` is a free-form list of
    /// "what got removed/changed" — used for telemetry, not for
    /// caller-side decisions.
    Filter {
        response: Response,
        dropped: Vec<String>,
    },

    /// Validator considers the response unacceptable. Surfaces as
    /// `ProviderError::ValidationFailed`; existing RetryMiddleware
    /// decides whether to retry based on the `retriable` flag.
    Reject {
        reason: String,
        retriable: bool,
    },

    /// Response unchanged. Validator wants to record per-call metrics
    /// (e.g. "this finding count was unusually low") that downstream
    /// code can read from `RequestContext.attributes` or from
    /// `Response.validation_summary` (see §4.2).
    Annotate {
        metrics: HashMap<String, serde_json::Value>,
    },
}
```

**Contract for `validate`:**

- Must be a pure function, same input → same output. **Forbidden to depend on external state** (unless that state was captured at validator construction, e.g. `RuleIdWhitelistValidator(allowed: HashSet<String>)`)
- Must be panic-safe; a validator panic is treated as Reject, error_message is the panic reason, does not block other validators from running
- Not allowed to await async work; validator is a synchronous function. Anything needing async IO (fetch remote data / RPC) goes to evaluator (Doc 16), where async is natural

### 3.2 `Filter` field rationale

Why Filter must return a complete `Response` rather than just `transformed_text: String`:

`Response` is more than text. It contains `text`, `thinking`, `tool_calls`, `usage`, `stop_reason`, `telemetry`. A validator may want to:

- Modify `tool_calls` ("this tool is hallucinated, drop it")
- Adjust `usage` (token count should drop accordingly after filter)
- Rewrite `stop_reason` (after filtering large content, "end_turn" may no longer be accurate)
- Rewrite `thinking` (if there's also content to filter from the thinking channel)

Letting the caller return a complete, self-consistent Response is cleanest — the validator itself guarantees internal consistency. `dropped: Vec<String>` is an audit trail, used by telemetry / debug, not control flow.

### 3.3 `ProviderError::ValidationFailed`

```rust
// Location: tars-types::error
pub enum ProviderError {
    ...existing variants...

    /// An OutputValidator rejected the response. This is the bridge
    /// between the validation layer and the existing retry/error
    /// handling — surfaces through normal error class machinery.
    #[error("validation failed: {validator}: {reason}")]
    ValidationFailed {
        validator: String,
        reason: String,
        retriable: bool,
    },
}

impl ProviderError {
    pub fn class(&self) -> ErrorClass {
        match self {
            ...existing arms...
            ValidationFailed { retriable: true, .. } => ErrorClass::Retriable,
            ValidationFailed { retriable: false, .. } => ErrorClass::Permanent,
        }
    }
}
```

**Python exposure (tars-py::errors)**:
- `kind = "validation_failed"`
- `validator: str` — name of the validator that triggered
- `reason: str` — rejection reason (same as message)
- `is_retriable: bool` — existing field, auto-populated

Downstream consumer handles in one line: `except tars.TarsProviderError as e: if e.kind == "validation_failed": ...`.

---

## 4. ValidationMiddleware

### 4.1 Implementation skeleton

```rust
// Location: tars-pipeline::validation
pub struct ValidationMiddleware {
    validators: Vec<Box<dyn OutputValidator>>,
}

impl ValidationMiddleware {
    pub fn new(validators: Vec<Box<dyn OutputValidator>>) -> Self {
        Self { validators }
    }
}

impl LlmService for ValidationMiddleware {
    async fn call(
        self: Arc<Self>,
        req: ChatRequest,
        ctx: RequestContext,
    ) -> Result<LlmEventStream, ProviderError> {
        // Telemetry: this layer was traversed.
        if let Ok(mut t) = ctx.telemetry.lock() {
            t.layers.push("validation".into());
        }

        // Drain inner stream into a complete Response. We can't
        // validate token-by-token; validators need the whole response.
        let inner_stream = self.inner.clone().call(req.clone(), ctx.clone()).await?;
        let mut builder = ChatResponseBuilder::new();
        let mut events_held = Vec::new();
        let mut s = inner_stream;
        while let Some(ev) = s.next().await {
            let ev = ev?;
            events_held.push(ev.clone());
            builder.apply(ev);
        }
        let mut response = builder.finish();

        // Run validators in order. Filter chains; first Reject short-circuits.
        let mut summary = ValidationSummary::default();
        for v in &self.validators {
            let outcome = v.validate(&req, &response);
            match outcome {
                ValidationOutcome::Pass => {
                    summary.outcomes.insert(v.name().into(), OutcomeSummary::Pass);
                }
                ValidationOutcome::Filter { response: new_resp, dropped } => {
                    summary.outcomes.insert(v.name().into(),
                        OutcomeSummary::Filter { dropped: dropped.clone() });
                    response = new_resp;
                    // Subsequent validators see the filtered response.
                }
                ValidationOutcome::Reject { reason, retriable } => {
                    return Err(ProviderError::ValidationFailed {
                        validator: v.name().to_string(),
                        reason,
                        retriable,
                    });
                }
                ValidationOutcome::Annotate { metrics } => {
                    summary.outcomes.insert(v.name().into(),
                        OutcomeSummary::Annotate { metrics });
                }
            }
        }

        // Stash the summary on Response (see §4.2). Re-emit the
        // (potentially Filtered) response as a stream so downstream
        // consumers see a stream-shaped flow.
        attach_validation_summary(&mut response, summary);
        let final_stream = response_to_stream(response);
        Ok(Box::pin(final_stream))
    }
}
```

### 4.2 `Response.validation_summary`

New field, populated after each call:

```rust
pub struct Response {
    ...existing fields (text, thinking, usage, stop_reason, telemetry)...

    /// Per-call validation outcomes, populated by ValidationMiddleware.
    /// Empty when no ValidationMiddleware was in the pipeline.
    pub validation_summary: ValidationSummary,
}

#[derive(Clone, Debug, Default)]
pub struct ValidationSummary {
    /// One entry per validator that ran, in registration order.
    pub outcomes: BTreeMap<String, OutcomeSummary>,
    pub validators_run: Vec<String>,
    pub total_wall_ms: u64,
}

#[derive(Clone, Debug)]
pub enum OutcomeSummary {
    Pass,
    Filter { dropped: Vec<String> },
    Annotate { metrics: HashMap<String, serde_json::Value> },
    // Reject doesn't appear in summary — the call returned Err instead.
}
```

Caller view (Python):

```python
r = pipeline.complete(...)
r.text                    # final text (post-Filter)
r.usage                   # token counts
r.telemetry               # cache_hit / retry_count (Stage 4)
r.validation_summary      # validator outcomes (added by this doc)
   .outcomes              # dict[validator_name, outcome_dict]
   .validators_run        # list[str] — execution order
   .total_wall_ms         # total validation time
```

No conflict with Stage 4 `Response.telemetry` — one is an infra metric, the other a semantic metric, both coexist:

| Field | Content | Producer |
|---|---|---|
| `Response.telemetry.cache_hit` | Cache hit (runtime fact) | TelemetryMiddleware |
| `Response.telemetry.retry_count` | Retry count (runtime fact) | RetryMiddleware |
| `Response.validation_summary.outcomes["x"]` | Disposition of validator x (semantic judgement) | ValidationMiddleware |

---

## 5. Built-in validators

v1 ships a set of deterministic validators covering 80% of cases. LLM-as-judge style validators are **not** provided as built-ins — see §10 anti-patterns.

### 5.1 `JsonShapeValidator`

```rust
pub struct JsonShapeValidator {
    schema: serde_json::Value,
    on_fail: ShapeFailMode,  // Reject | Annotate
}

pub enum ShapeFailMode { Reject { retriable: bool }, Annotate }
```

Behavior: try `serde_json::from_str(&resp.text)`, then validate using the [jsonschema](https://crates.io/crates/jsonschema) crate.
- Parse or validate fails → according to `on_fail`, Reject or Annotate (`shape_violation: true`)
- Parse succeeds → Pass

Why this is the first v1 built-in: the critic output of the downstream consumer is already strict JSON, the schema is already defined, and reuse across consumers is highest.

### 5.2 `RuleIdWhitelistValidator`

Upgrade of the downstream consumer's inline implementation.

```rust
pub struct RuleIdWhitelistValidator {
    json_path: String,           // e.g. "$.findings[*].rule_id"
    allowed: HashSet<String>,
    on_unknown: UnknownIdMode,   // DemoteToAdHoc | Reject | Annotate
}
```

Behavior: parse text as JSON → JSONPath extracts all rule_ids → compare against `allowed` set → dispose per `on_unknown`.

`DemoteToAdHoc` mode: rewrite all rule_ids not in the whitelist to `"ad-hoc"`, record original value in the evidence field (matches the downstream consumer's current inline behavior). **Filter outcome** — return the rewritten Response.

### 5.3 `MaxLengthValidator`

```rust
pub struct MaxLengthValidator {
    field: ResponseField,        // Text | Thinking | ToolCallsCount
    max: usize,
    on_exceed: LengthFailMode,   // Reject | TruncateAndAnnotate
}
```

Defends against prompt injection / runaway generation — caller doesn't want critic output exceeding N KiB.

### 5.4 `RegexBannedValidator`

```rust
pub struct RegexBannedValidator {
    patterns: Vec<regex::Regex>,
    on_match: BannedMatchMode,   // Reject | Filter
}
```

Output must not contain `password=`, API key shapes, PII patterns, etc. Filter mode removes matched fragments; Reject mode rejects the whole call.

### 5.5 `EvidenceTagValidator` (downstream-consumer-specific, shown as example, not a built-in)

```rust
pub struct EvidenceTagValidator {
    json_path: String,           // "$.findings[*].evidence"
    required_keys: Vec<String>,  // ["kind", "axis", "action", "confidence"]
    on_missing: MissingTagMode,  // Reject | Annotate
}
```

**Not a built-in** — downstream consumer-specific evidence schema. But the trait shape is identical, the consumer registers it itself. Doc gives the implementation example for reuse.

---

## 6. Python bindings

### 6.1 Python form for implementing OutputValidator

Reuse the Stage 3 PyTool PyO3 pattern. Python writes a base class, Rust wraps it as `OutputValidator`.

```python
import tars

class RuleIdWhitelistValidator(tars.OutputValidator):
    def __init__(self, allowed: set[str], json_path: str = "$.findings[*].rule_id"):
        super().__init__(name="rule_id_whitelist")
        self.allowed = allowed
        self.json_path = json_path

    def validate(self, req, resp):
        # parse resp.text as JSON, walk JSONPath, find unknowns
        try:
            data = json.loads(resp.text)
        except json.JSONDecodeError as e:
            return tars.Reject(reason=f"not valid JSON: {e}", retriable=True)

        unknowns = []
        for finding in data.get("findings", []):
            rid = finding.get("rule_id")
            if rid and rid not in self.allowed:
                unknowns.append(rid)
                # demote in place
                finding["evidence"] = f"hallucinated_rule_id={rid}; " + finding.get("evidence", "")
                finding["rule_id"] = "ad-hoc"

        if not unknowns:
            return tars.Pass()

        # Build a new Response with the rewritten text.
        new_resp = resp.with_text(json.dumps(data))
        return tars.Filter(response=new_resp, dropped=unknowns)
```

Four outcome factory functions: `tars.Pass()` / `tars.Filter(response, dropped)` / `tars.Reject(reason, retriable)` / `tars.Annotate(metrics)`.

### 6.2 Registering with Pipeline

```python
p = (
    tars.Pipeline.builder("qwen_coder_local")
        .add_validator(RuleIdWhitelistValidator(KNOWN_IDS))
        .add_validator(MaxLengthValidator(field="text", max=50_000))
        .build()
)
```

Requires PyO3 to expose `Pipeline.builder()` — currently only `Pipeline.from_default(id)` and `from_str(toml, id)` exist; a builder API needs to be added (B-6c is already on the TODO list, fold it into this work).

### 6.3 Sync safety / GIL

The PyO3 wrapper (`PyValidatorAdapter`) acquires the GIL on `validate` → calls Python's `validate(req, resp)` → converts the outcome to a Rust enum. Identical shape to PyTool (see `tars-py/src/session.rs:267`).

Note: **Validator is synchronous, async work is not allowed**. In Python, you cannot use `async def validate`; if your validator needs IO, either push the IO to construction time (preload), or rewrite as an evaluator (Doc 16).

---

## 7. Downstream consumer migration path

### 7.1 Status quo (commit `1fe6cbc`)

```python
# inside app/core/critic_agent.py
self._known_rule_ids: set[str] = ...  # built at load time
# end of scan_file:
if self._known_rule_ids:
    for uid, finding in parsed.items():
        rid = finding.get("rule_id", "")
        if rid and rid not in self._known_rule_ids:
            finding["rule_id"] = "ad-hoc"
            finding["evidence"] = f"hallucinated_rule_id={rid}; " + (finding.get("evidence") or "")
            demoted += 1
```

Characteristics:
- Inlined in critic_agent, tightly coupled with the LLM call
- Used only by the downstream consumer itself
- Tests live in downstream consumer tests
- Single mode (demote-to-ad-hoc)

### 7.2 After migration

```python
# new file consumer/validators.py
import tars

class ArcRuleIdWhitelistValidator(tars.OutputValidator):
    def __init__(self, rubric_paths: list[str]):
        super().__init__(name="consumer_rule_id_whitelist")
        self.allowed = RubricParser.known_rule_ids(rubric_paths)

    def validate(self, req, resp):
        # ... call the Python equivalent of the built-in RuleIdWhitelistValidator logic
```

```python
# app/core/critic_agent.py adjusted
self._pipeline = (
    tars.Pipeline.builder("qwen_coder_local")
        .add_validator(ArcRuleIdWhitelistValidator(rubric_paths))
        .build()
)
# scan_file no longer post-filters — Validator already runs inside Pipeline
parsed = self._pipeline.complete(...).text  # already Filter'd
```

Migration benefits:
- 50 lines of critic_agent code → 0 lines (moved to validator class, one line of `add_validator`)
- Tests: validator unit tests + downstream consumer integration tests verify together
- Reuse: future tools wanting this just import the same validator

### 7.3 Migration cost / what stays unchanged

- **Dogfood data schema unchanged** — the `hallucinated_rule_id=...` marker in the `evidence` field is preserved, downstream metrics remain compatible
- Time: ~30 min (validator class ~30 lines, pipeline assembly ~3 lines, delete old inline ~20 lines, test adjustments ~10 lines)
- Risk: low — equivalent behavior, unit-test covered

---

## 8. Order / composition semantics

### 8.1 Execution order = registration order

`add_validator(A)`, `add_validator(B)`, `add_validator(C)` → execution order A → B → C.

### 8.2 Filter chains

Each Filter validator sees the Response after the previous Filter.

```rust
// validator A returns Filter(resp=A')
// validator B sees A', returns Filter(resp=B')
// final response: B'
```

### 8.3 Reject short-circuits

The first Reject immediately aborts the entire validation chain. Subsequent validators do not run.

Rationale: Reject already triggers retry / fail, no point continuing to accumulate metrics.

### 8.4 Annotate does not affect control flow

Annotate writes metrics to `summary.outcomes`, response unchanged, subsequent validators run normally.

### 8.5 Recommended ordering

**Cheap deterministic ones first, expensive ones later**:

1. `JsonShapeValidator` (parse JSON) — parse fail Rejects immediately, doesn't waste later validators' work
2. `MaxLengthValidator` (O(N) char count)
3. `RuleIdWhitelistValidator` (O(N) findings count)
4. `EvidenceTagValidator` (O(N) findings count)
5. `RegexBannedValidator` (regex compiled ahead of time, match O(N))

**Validators with dependencies must come after their dependencies**: e.g. `RuleIdWhitelistValidator` depends on JSON being parseable, must come after `JsonShapeValidator`. Documented clearly, caller is responsible for correct ordering — no explicit dependency graph (YAGNI).

---

## 9. Telemetry & integration

### 9.1 Layer trace

ValidationMiddleware adds a "validation" tag to `Response.telemetry.layers`. Caller sees the layer chain as `["telemetry", "cache_lookup", "retry", "validation", "provider"]`.

### 9.2 Telemetry on OutputValidator failure

When a validator panics or throws an unexpected error, ValidationMiddleware converts it to Reject + writes a tracing warn. **Don't let validator bugs silently swallow the response**.

### 9.3 Cooperation with Doc 16 evaluation

Validation Annotate metrics are written to `Response.validation_summary`. Doc 16 evaluation dimension scores are written to `EventStore` `EvaluationScored` events. The two don't overlap:

- Validation Annotate = "snapshot for this single call", immediately readable
- Evaluation = "trend across multiple calls in a time window", queried from EventStore

Caller picks one based on use case:
- Want to immediately decide next action ("score < 0.5 go fallback") → Validation Annotate
- Want to see trends / feed dashboards → Evaluation (Doc 16)

---

## 10. Anti-patterns (explicitly NOT to do)

### 10.1 Don't stuff evaluation into a validator

```python
# ❌ Anti-example
class CriticQualityValidator(tars.OutputValidator):
    def validate(self, req, resp):
        # Call an LLM-as-judge to score the critic
        judge_score = llm.complete(f"Rate this critique 0-1: {resp.text}")
        if judge_score < 0.7:
            return tars.Reject(...)
```

Problems:
1. One extra LLM call per call — cost doubles, latency doubles
2. LLM-judge is non-deterministic, validation loses reproducibility
3. Mixes the validation gate with the evaluation dashboard — when scores drop you can't tell whether quality genuinely declined or the judge model itself jittered

Correct approach: judging critique quality is an **evaluation** dimension, build it as a Doc 16 async evaluator with results fed back via EventStore, not participating in the validation gate.

### 10.2 Don't do async IO inside a validator

```python
# ❌ Anti-example
class FactCheckValidator(tars.OutputValidator):
    async def validate(self, req, resp):       # interface forbids async
        facts = await fetch_knowledge_base(...)
        ...
```

Problem: the validator contract is synchronous — blocking work per call. async IO (database / RPC) extends the critical path.

Correct approach: Doc 16's async evaluator supports this naturally.

### 10.3 Don't use a validator as a config toggle

```python
# ❌ Anti-example
class DebugValidator(tars.OutputValidator):
    def validate(self, req, resp):
        if os.getenv("ARC_DEBUG"):
            print(f"DEBUG: {resp}")
        return tars.Pass()
```

Problem: validator used for debug side effects — but it runs on every call, side effects are uncontrollable. Debug should go through logging / `tracing::*`.

### 10.4 Don't let Filter implicitly change the schema

```python
# ❌ Anti-example
class TruncateAllStringsValidator(tars.OutputValidator):
    def validate(self, req, resp):
        # truncate every string field to 100 chars
        return tars.Filter(response=truncated, dropped=["..."])
```

Problem: the Response.text the caller receives no longer matches the original prompt's schema expectation — downstream parser may break. Filter should **delete/replace illegal parts**, not **uniformly rewrite all fields**.

Correct approach: MaxLengthValidator targets one field explicitly, truncation behavior is predictable.

---

## 11. Implementation path

Follows the milestone style of [Doc 14 §9 Implementation Path](./14-implementation-path.md). This doc corresponds to **M9 wave 1**.

### 11.1 Phase breakdown

| Phase | Content | Estimate |
|---|---|---|
| **W1.1** | `OutputValidator` trait + `ValidationOutcome` enum + `ProviderError::ValidationFailed` variant | 0.5 d |
| **W1.2** | `ValidationMiddleware` impl + integration into Pipeline builder + drain-then-emit stream wrapping | 1 d |
| **W1.3** | `Response.validation_summary` field + tars-py exposure + `tars.Pipeline.builder()` API | 0.5 d |
| **W1.4** | Built-in validators: `JsonShapeValidator` + `RuleIdWhitelistValidator` + `MaxLengthValidator` + `RegexBannedValidator` | 1 d |
| **W1.5** | PyO3 cross-language `tars.OutputValidator` base class + `tars.Pass/Filter/Reject/Annotate` factories | 1 d |
| **W1.6** | Unit tests + integration tests + downstream consumer migration example + CHANGELOG | 0.5 d |
| **Total** | | **~4.5 d** |

### 11.2 Immediate actions after landing

1. Downstream consumer migrates `_known_rule_ids` out (B-15 TODO already noted, see consumer repo commit `1fe6cbc`)
2. Downstream consumer hooks up `MaxLengthValidator` (defends against prompt-injection output blowup)
3. Write an `EvidenceTagValidator` (downstream-consumer specific) as a plugin example, lands in the downstream consumer repo's `consumer/validators.py`
4. tars side monitors validation overhead — TelemetryMiddleware already includes validation time in `pipeline_total_ms`, verify < 5% pipeline overhead

### 11.3 Not in W1 (push to W2 or cut)

- Config-driven plugin discovery (`[validators]` TOML section) — static registration suffices for 99%
- Streaming-aware validator (judge mid token-by-token whether to Reject) — most scenarios don't need this, YAGNI
- A separate `tars-validation` crate — only 4 built-ins, fits as `tars-pipeline::validation` submodule

---

## 12. Cross-doc references

- **Doc 02 Middleware Pipeline** — the ValidationMiddleware defined here is layer 4 (between Retry and Provider)
- **Doc 12 API Specification** — `Pipeline::builder().add_validator(...)` API is normalized there
- **Doc 16 Evaluation Framework** — evaluation (multi-dimensional scoring / time series / datasets) is a separate concern, see that doc
- **Doc 04 Agent Runtime** — how an Agent assembles a Pipeline with ValidationMiddleware is covered there

---

## 13. Open questions

### 13.1 Explicit dependency graph between validators?

Current: caller is responsible for ordering. E.g. `RuleIdWhitelistValidator` assumes JSON is already parseable, so it's placed after `JsonShapeValidator`.

Alternative: trait adds `fn depends_on() -> Vec<&str>`, runtime topological sort.

Verdict: YAGNI. Manual ordering is fine within 8 validators; reconsider beyond that scale.

### 13.2 Streaming validator?

Current: validator gets the complete Response.

Alternative: `fn validate_partial(&self, partial: &PartialResponse) -> Option<ValidationOutcome>` — validator decides mid-stream whether to early-reject (e.g. max_length aborts immediately when threshold hit).

Verdict: not in v1. max_length can be capped at the provider layer via `max_output_tokens`, no validator needed. Streaming-aware is an optimization, not a new capability.

### 13.3 Cross-call validator state?

Current: each call is independent, validator is stateless.

Alternative: validator holds `Arc<Mutex<State>>`, accumulates across calls (e.g. "ad-hoc rate over the past 100 calls", starts Reject above a threshold).

Verdict: **this is evaluation, not validation** — see Doc 16. Validator must be a stateless pure function.

### 13.4 Per-tenant configuration?

Current: ValidationMiddleware fixes the validator list at Pipeline construction time.

Alternative: look up per-tenant config from `RequestContext.tenant_id`, dynamically select the validator set.

Verdict: defer until Multi-tenant lands (Doc 06 §3). Current Pipeline is single-tenant assumption.
