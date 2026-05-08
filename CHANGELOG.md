# CHANGELOG

Roadmap-level shipped items, organized by milestone (Doc 14). Entries
record **what shipped + commit ref**, plus a short **why** when the
decision wasn't obvious from the diff. The intent: a reader landing
on this repo gets a one-page tour of "where are we" without grepping
git log or wading through TODO.md's deferred / frozen lists.

For things deliberately **not** done, see [TODO.md](./TODO.md):
- Overengineering items (O-1..O-10) — scaffolds we
  carry on a trigger-or-delete contract.
- Audit deferrals (A-1, A-4) — non-critical findings revisitable
  on touch.
- Doc-N gap items (D-1..D-13) — explicit deferred / frozen entries
  per Doc N's full surface vs. what's wired today.

For commit-level detail (per-file diffs, exact line numbers): `git log`
is authoritative. This file aggregates.

---

## M8 — Python bindings (`tars-py`) (in progress 2026-05-04)

PyO3 + maturin-built wheel exposing tars to Python (and, via the
same shape, future TS / Go bindings). First non-Rust consumer is
downstream consumer, migrating from a hand-rolled `LLMClient` + 80-line `Session`
to `tars.Pipeline` + `tars.Session`. Long-term downstream consumer goes full Rust
but Python is a permanent first-class surface — design treats it as
such, not as a throwaway scaffold.

### Stage 1 — `from_str` + typed exceptions (`<unreleased>`)

- **`Pipeline.from_str(toml, provider_id)` / `Provider.from_str`** —
  inline TOML constructors, no tmpfile round-trip required for
  tests / programmatic config. Backed by `ConfigManager::load_from_str`
  which already existed in `tars-config`.
- **Exception hierarchy** — `TarsError` (base) → `TarsConfigError` /
  `TarsProviderError` / `TarsRuntimeError`. `TarsProviderError`
  carries structured `kind` (`"rate_limited"`, `"auth"`, `"network"`,
  `"unknown_tool"`, ...), `retry_after: float | None`,
  `is_retriable: bool`, plus optional `tool_name` for the
  `unknown_tool` variant. Caller branches without parsing message
  strings. Mapping lives in `tars-py/src/errors.rs`.

### Stage 2 — User-level config + builtins + `tars init` (`<unreleased>`)

- **`~/.tars/config.toml` is the user-level default config path.**
  Follows developer-tool convention (`~/.gitconfig`, `~/.cargo/`,
  `~/.aws/`, `~/.claude/`) rather than XDG / `~/Library/Application
  Support`. Same path on macOS / Linux / Windows. Implementation in
  new `tars-config::paths::default_config_path()`. Python: `tars.
  default_config_path()`.
- **`Pipeline.from_default(provider_id)` / `Provider.from_default`** —
  zero-arg config resolution. downstream consumer + future tools call this and don't
  need to know the path.
- **Built-in provider merge at load time** — `ConfigManager::load_*`
  now layers `built_in_provider_defaults()` under the user TOML.
  Empty `[providers]` is no longer a validation error; users can
  resolve `mlx` / `vllm` / `openai` / etc. by id with zero TOML.
  `Config.user_provider_ids: HashSet<ProviderId>` captures the
  pre-merge user-declared set so the CLI's implicit-pick logic
  doesn't see "ambiguous (8 builtins)" when the user wrote one
  provider — we filter to user-declared for that path.
- **`vllm` builtin** added (was missing — `mlx` / `llamacpp` /
  `openai` / `anthropic` / `gemini` / `claude_cli` / `gemini_cli`
  were already there, vLLM was the lone gap given that downstream consumer ran on
  vLLM historically).
- **`tars init` CLI** — bootstraps `~/.tars/config.toml` with a
  starter template (LM Studio, MLX, vLLM commented; cloud providers
  + auth-via-env-var commented). `--force` to overwrite, `--path`
  to redirect (test fixtures).

### Stage 3 — `Session` (multi-turn + tool loop + atomic rollback) (`<unreleased>`)

Layer-2 stateful container above `Pipeline`. Drop-in compatible
with downstream consumer's existing `Session(client, system, max_history_chars)`
shape; adds turn-aware trim and the tool-dispatch auto-loop downstream consumer
hadn't yet written.

- **`tars-runtime::Session`** — `Session::new(pipeline, capabilities,
  SessionOptions { system, budget, tools, default_max_output_tokens,
  model })`. PyO3 wrap as `tars.Session`.
- **`Turn`** holds the full message chain for one logical exchange:
  leading user → 0..N (assistant tool_use → user tool_result) rounds
  → final assistant text. `is_complete()` validates these
  invariants at turn-close + on future deserialize. Mid-loop
  incompleteness is valid in-flight state; `is_complete()` on a
  mid-loop turn correctly returns false.
- **Budget** — three modes. `Chars(usize)` (default, 400_000 ≈
  100k tokens at 4:1 ratio, matches downstream consumer's known-good default).
  `Tokens { limit, tokenizer: Arc<dyn Tokenizer> }` (opt-in
  precise). `ContextRatio(f32)` — sugar that reads
  `capabilities.max_context_tokens × ratio` so the caller doesn't
  have to pick a number.
- **Trim runs exactly once per `send()` entry**, before any model
  call. Drops oldest *whole* turns until under budget — never
  splits a turn (would orphan a `tool_use` without its
  `tool_result`, which Anthropic + OpenAI both 400 on at the wire).
  Auto-loop continuations (multi-round tool use within one turn)
  do NOT re-trim. Enforced by a release-mode `assert_eq!` on the
  turn count delta — programmer-error class, kept in prod because
  silent desync of `turns` is harder to diagnose than a panic.
- **Atomic Turn rollback via Drop guard** — `TurnGuard` uses the
  scoped commit-pattern: default Drop = rollback (`turns.truncate
  (boundary)`); success path calls `guard.commit()` which
  `mem::forget`s the guard. Catches `?` early-return, panic, and
  tokio cancellation uniformly. Strictly safer than an
  `armed: bool` flag because there's no way to forget a single
  `armed = false` and silently keep a half-Turn.
- **Auto tool loop** — when `ToolRegistry` is set, `send()`
  dispatches tools and re-invokes the model until the model
  returns a text-only reply. Parallel tool_calls are dispatched
  in order and packaged into one user message with N
  `tool_result` blocks (Anthropic's protocol requires all parallel
  results in one message). Manual mode is reachable by leaving the
  registry empty and consuming `Response.tool_calls` from caller
  side.
- **`UnknownTool { name }` ProviderError variant** — surfaced when
  the model emits a `tool_use` for a tool not in the registry.
  Class = `Permanent` (retrying is futile — model will keep
  emitting the same name; caller's registry needs the tool added).
  Python `kind = "unknown_tool"` + `e.tool_name` attribute lets
  downstream consumer render "did you forget to register tool X?".
- **BudgetWarning de-dup** — first mid-turn over-budget emits a
  `tracing::warn!` once; subsequent occurrences in the same
  Session are counted silently. `Drop for Session` emits a summary
  if count > 1. `reset()` / `fork()` clear the counter (logical
  fresh start). No time-based rate-limit (agentic-loop firing
  cadence is too irregular for time windows to mean anything).
- **`fork()` / `reset()`** — fork is cheap-clone of conversation
  state (messages, budget, tools registry); `reset()` clears
  history but preserves system / model / budget / tools.
- **`history_version: u64`** counter on Session, bumped on visible
  history mutations (successful send, reset). NOT bumped on
  rollback (truncating back to pre-send is observably
  unchanged). NOT bumped during in-flight tool loops. Adopted
  from an established `ContextManager.history_version`. Useful for
  cache invalidation and `(session_id, history_version)` log
  correlation. `fork()` preserves the parent value so caches can
  recognize shared prefixes.

### Routing capability pre-flight check (B-31, `<unreleased>`)

External code review (2026-05-05) found `RoutingService::call` was
dispatching requests to fallback candidates **without checking
capabilities**. Requests with tools / vision / thinking / oversized
context against a non-supporting candidate would silently drop
features or wire-400. Fixed by adding a local pre-flight check.

Reviewed twice; second-pass review caught 5 design improvements
(Vec<String>→typed enum, `#[non_exhaustive]`, context-window check,
PyO3 expose, structured tracing fields) — all fixed before unreleased
flips. Final shape:

- **`tars_types::CompatibilityReason`** — typed enum with 6 variants
  (`ToolUseUnsupported{tool_count}` / `StructuredOutputUnsupported` /
  `ThinkingUnsupported{mode}` / `VisionUnsupported` /
  `ContextWindowExceeded{estimated_prompt_tokens, max_context_tokens}` /
  `MaxOutputTokensExceeded{requested, max}`). Each variant carries
  structured fields for programmatic branching. `Display` impl gives
  human-readable messages. `kind() -> &'static str` returns stable
  snake_case tag for telemetry / metric labels (independent of enum
  variant names which we may rename internally). Marked
  `#[non_exhaustive]` so future variants (e.g. `MaybeWithCaveat`)
  don't break SemVer.
- **`tars_types::CompatibilityCheck`** — 2-state verdict
  (`Compatible` / `Incompatible { reasons: Vec<CompatibilityReason> }`).
  Also `#[non_exhaustive]`.
- **`tars_types::ChatRequest::compatibility_check(&Capabilities)
  -> CompatibilityCheck`** — aggregates ALL incompatibilities in one
  pass (no early-exit on first failure, so caller sees the full list).
  Includes context-window overflow check via `chars/4` heuristic
  (partial fix for B-32 — full fix needs tokenizer / D-5 unfreeze).
- **`RoutingService::call` integration** — candidates that fail the
  check are skipped with structured `tracing::warn!` (fields:
  `candidate_id`, `chain_position`, `chain_total`, `trace_id`,
  `reason_kinds: Vec<&'static str>`) — log-aggregation friendly. When
  *all* candidates are skipped, returns
  `ProviderError::InvalidRequest("no candidate could honour request
  capabilities; skipped: <id>: [<Display>]; …")` (Permanent class).
- **Python surface (`tars-py`)**:
  - `Pipeline.check_compatibility(model=..., user=..., tools=...,
    max_output_tokens=..., ...)` — pre-flight from Python WITHOUT
    making a model call. Lets downstream consumer short-circuit to a different
    provider before incurring a network round-trip.
  - `CompatibilityResult` pyclass with `.is_compatible: bool`,
    `.reasons: list[CompatibilityReason]`, `__bool__` ergonomic, and
    informative `__repr__`.
  - `CompatibilityReason` pyclass with `.kind: str`, `.message: str`,
    `.detail: dict | None` (kind-specific structured fields).
- **Test coverage**: 17 tests in `tars-types::chat::tests` (each
  capability axis + multi-reason aggregation + boundary cases:
  baseline-caps + zero-caps adversarial + Display rendering); 3
  routing-level tests (skip-and-try-next / all-skipped /
  pass-through-when-compatible); end-to-end Python smoke verifying
  bool/repr/detail surface.
- **Why 2-state not 3-state** (vs codex's `SafetyCheck`): per-candidate
  check doesn't have global view. Routing layer aggregates skip
  reasons across the chain and synthesizes the "no candidate works"
  verdict — that's where global-permanent rejection belongs, not in
  the per-candidate helper.

### Lightweight `check_capabilities_for` config-time API (B-31 v3, `<unreleased>`)

Third-pass review pointed out `check_compatibility(model, user, ...)`
requires inventing a real prompt to query — wasteful for config-time
"does this provider support tools at all?" sanity checks. Added a
declarative companion API.

- **`tars_types::CapabilityRequirements`** — declarative requirements
  struct (`requires_tools` / `requires_vision` / `requires_thinking`
  / `requires_structured_output` / `estimated_max_prompt_tokens` /
  `estimated_max_output_tokens`). All fields default to "I don't
  need this"; setting `estimated_max_*_tokens=0` disables that
  check (caller doesn't yet know).
- **`tars_types::Capabilities::check_requirements(&Requirements)
  -> CompatibilityCheck`** — same `CompatibilityCheck` verdict as
  `ChatRequest::compatibility_check`, so downstream branching code
  is shared. Aggregates ALL incompatibilities (no early-exit).
- **`tars-py::Pipeline.check_capabilities_for(requires_tools=False,
  ...)`** — Python config-time API; no real prompt required.
- **Use case (downstream consumer role-init pattern)**:
  ```python
  for role, provider_id in role_to_provider.items():
      p = tars.Pipeline.from_default(provider_id)
      r = p.check_capabilities_for(
          requires_tools=True,
          requires_thinking=(role == "planner"),
          estimated_max_output_tokens=8_000,
      )
      if not r:
          log.fatal(f"role={role} can't satisfy: {[x.kind for x in r.reasons]}")
          sys.exit(1)
  ```
  Avoids the "configure → fail at runtime → fall back" loop. Smoke
  shows real misconfig (`qwen_coder_local` lacks thinking →
  detected at startup, not after first request).
- **Test coverage**: 5 unit tests (default-empty / tool-axis / full-set
  aggregation / 0-means-skip / full-caps-passes-everything).

### Typed `NoCompatibleCandidate` + `TarsRoutingExhaustedError` subclass (B-31 v4, `<unreleased>`)

Fourth-pass review: routing-exhausted error currently arrives as a
generic `TarsProviderError(kind="invalid_request")` with the skipped
candidate list mashed into the message string. downstream consumer would have to
regex-parse the message to react. Replaced with end-to-end typed
data + dedicated exception subclass.

- **`tars_types::ProviderError::NoCompatibleCandidate { skipped:
  Vec<(ProviderId, Vec<CompatibilityReason>)> }`** — new variant
  carrying the typed skipped list. Class = `Permanent`. Display
  message: `"no candidate could honour request capabilities; tried
  N providers"`.
- **`RoutingService::call`** — now returns this variant when all
  candidates were skipped via capability pre-flight. Routing tests
  updated to match on `NoCompatibleCandidate { skipped }` directly
  rather than parsing strings.
- **`tars-py::TarsRoutingExhaustedError`** — new Python exception
  subclass: `TarsError → TarsProviderError → TarsRoutingExhaustedError`.
  - Caller branch: `except TarsRoutingExhaustedError` for typed
    access; `except TarsProviderError` for generic catch-all
    (`isinstance` still matches due to subclass).
  - Attribute `e.skipped_candidates: list[tuple[provider_id_str,
    list[CompatibilityReason]]]` — re-uses the same
    `CompatibilityReason` Python class as `Pipeline.check_compatibility`
    so caller's reason-handling code is uniform.
- **`provider_kind`** mapping adds `"no_compatible_candidate"` for
  retry middleware telemetry / Python `e.kind` access.
- **Why subclass not attribute**: `hasattr(e, "skipped_candidates")`
  on generic TarsProviderError adds a check noise + tempts callers
  to forget. Subclass + `isinstance` is idiomatic Python.

### Typed `CapabilityRequirements` pyclass (B-31 v5, `<unreleased>`)

Fifth-pass review: `Pipeline.check_capabilities_for(**kwargs)`'s
loose-kwargs API was as-untyped-as-Any. Field name typo
(`requires_struuctured_output=True`) silently accepted by `**unpack`,
surfacing as a runtime "unexpected keyword argument" far from the
typo site. downstream consumer was about to mirror the field set as a local
dataclass, which works but creates drift risk on tars upgrades.

Shipped: typed pyclass as the single source of truth.

- **`tars.CapabilityRequirements`** — frozen pyclass with the same
  axes as `tars_types::CapabilityRequirements` (Rust). Construction
  via kwargs, fields enforced (typo → `TypeError` at construction
  call site, not deep in `Pipeline.complete`).
- **Methods**: `is_empty()` for the factory pattern (skip pre-flight
  if no axes set), `to_kwargs()` for incremental adoption (build
  typed, pass via existing kwargs API), `__eq__` + `__hash__` for
  `dict[CapabilityRequirements, role]` and set-member patterns,
  `__repr__` for debugging.
- **`Pipeline.check_capabilities(requirements)`** — new typed-input
  variant of `check_capabilities_for(**kwargs)`. Same
  `CompatibilityResult` return; chooses based on caller's source
  of truth (mid-call quick check vs role-init mapping table).
- **Both APIs preserved** — kwargs form `check_capabilities_for`
  remains for one-off inline use; typed `check_capabilities` is
  the recommended shape for `dict[role, requirements]` factories.
- **Single source of truth**: when tars adds a new capability axis,
  pyclass picks it up automatically on next wheel build. Consumers
  importing `tars.CapabilityRequirements` get the new field
  without touching their code; consumers mirroring locally would
  silently miss it.
- **Pattern reference (downstream consumer role-init)**:
  ```python
  from tars import CapabilityRequirements, Pipeline
  ROLE_REQUIREMENTS: dict[str, CapabilityRequirements] = {
      "critic":  CapabilityRequirements(requires_tools=True),
      "planner": CapabilityRequirements(requires_thinking=True),
      "agent":   CapabilityRequirements(),  # no specific reqs
  }
  for role, reqs in ROLE_REQUIREMENTS.items():
      if reqs.is_empty():
          continue
      p = Pipeline.from_default(provider_for_role[role])
      r = p.check_capabilities(reqs)
      if not r:
          log.fatal(f"{role} unsupported", missing=[x.kind for x in r.reasons])
          sys.exit(1)
  ```

### B-20 Wave 1 — Rust-only Validator framework (`<unreleased>`)

Implements [Doc 15 — Output Validation](./docs/architecture/15-output-validation.md)
Wave 1 (Rust side; PyO3 binding lands in W2). After-call validators
run between Retry and Provider; rejections surface as
`ProviderError::ValidationFailed` and bubble through normal
ErrorClass machinery.

- **`tars_types::ValidationOutcome`** — 4-variant enum
  (`Pass` / `Filter { response, dropped }` / `Reject { reason,
  retriable }` / `Annotate { metrics }`), `#[non_exhaustive]`.
- **`tars_types::ValidationSummary` + `OutcomeSummary`** —
  per-call aggregated record. Reject doesn't appear in summary
  (call returned Err, no Response to attach to).
- **`tars_types::ProviderError::ValidationFailed { validator,
  reason, retriable }`** — new variant. `retriable=true` →
  `ErrorClass::Retriable` (RetryMiddleware retries naturally);
  `retriable=false` → `Permanent`.
- **`tars_types::SharedValidationOutcome`** — Arc<Mutex<...>>
  side-channel on `RequestContext.validation_outcome`. Mirrors
  Stage 4's SharedTelemetry pattern. ValidationMiddleware writes
  the summary + post-Filter ChatResponse here; caller reads
  after stream drain. Avoids polluting the ChatEvent enum with
  end-of-stream metadata variants.
- **`tars_pipeline::OutputValidator` trait** — pure-function
  contract (`fn validate(req, resp) -> ValidationOutcome`).
  Document at trait level: deterministic, no IO, no side
  effects. Validators that need IO go to evaluator framework
  (Doc 16 / W3) where async + non-determinism are first-class.
- **`tars_pipeline::ValidationMiddleware`** — wraps inner stream:
  drains, runs validators in order, re-emits.
  - Empty validator list → pass-through, no drain.
  - Filter chains (each validator sees prior Filter's output).
  - Reject short-circuits (subsequent validators don't run).
  - Annotate accumulates metrics in summary, response unchanged.
  - Layer name "validation" appended to `RequestContext.telemetry.
    layers`.
- **3 built-in validators**:
  - `JsonShapeValidator` — `serde_json::from_str` parse check.
    Default `retriable=true` (model non-determinism); override
    via `with_retriable(false)` for permanent-shape rejections.
  - `NotEmptyValidator` — guards against empty responses (safety
    filter trips, token cutoff, abort). Field selectable
    (`Text` / `Thinking`).
  - `MaxLengthValidator` — defends against runaway generation /
    prompt injection. Two modes: `Reject` or `Truncate` (Filter
    in-place, drops tail with audit-list entry).
- **17 unit tests in `tars-pipeline::validation::tests`**: each
  built-in validator's Pass/Reject/Filter outcomes; middleware
  behaviour for chain-order + Filter chaining + Reject
  short-circuit + Annotate accumulation + empty-chain
  passthrough + layer trace.

**Known caveat**: ValidationMiddleware sits between Retry and
Provider per Doc 15 §2 (so retriable rejections retry through
RetryMiddleware). With Cache positioned outside Retry, cache
hits short-circuit before reaching Validation — validators
**do not rerun on cache hit** in this layout. The "cache stores
raw, hit reruns validator" design from earlier brainstorm
requires reordering layers (Validation outside Cache); deferred
to a follow-up after real multi-caller-cache trigger appears.
Single-caller / single-validator-chain configurations (downstream consumer's
current shape) work correctly.

### Output-Validation × Cache interaction rule (locked, B-20 W1 implementation)

Decision documented for B-20 Wave 1 implementation:
**cache stores raw (pre-validation) Response. Cache hit ALWAYS
reruns validators on cached payload.** Validators are pure
(same input → same output per Doc 15 §3.1 trait contract), so
re-running on cache hit is local CPU only — far cheaper than a
wire round-trip. Multi-caller sharing the same cache works
correctly because each caller's validator chain operates on raw
output.

**Validator failure does NOT bypass cache** — failed-validator
runs leave the raw response in cache. Repeated cache hits
deterministically fail validation (validator is pure). Caller
who wants force-fresh on validation fail uses an explicit
`skip_cache=True` kwarg (future). Eliminates cache-poisoning
worry while keeping the trait contract clean.

> **Note (W4, 2026-05-08)**: this contract was **not actually
> honored by W1's implementation** — `ValidationMiddleware` sat
> *inside* Cache in the onion, so Cache stored post-Filter events
> and cache hits short-circuited before Validation. the consumer raised the
> "single-validator-chain assumption" flag during 2026-05-08 dogfood
> prep; tars-side audit + failing regression tests confirmed the
> deeper bug. **Now correctly enforced after W4 below**: Validation
> moved OUTSIDE Cache.

### B-20 Wave 2 — PyO3 validator binding (`<unreleased>`)

Python validators attach as `[(name, callable), ...]` to
`Pipeline.{from_default,from_config,from_str}`. `PyValidatorAdapter`
bridges Python callbacks into the Rust `OutputValidator` trait;
`build_req_dict` / `build_resp_dict` flatten ChatRequest/ChatResponse
to plain dicts so the callable surface is `(req: dict, resp: dict)
→ outcome`. Four outcome pyclasses: `tars.Pass`, `tars.Reject`,
`tars.FilterText`, `tars.Annotate`.

Robustness contract: a buggy validator (raises a Python exception,
returns the wrong type) is caught by the adapter and translated
into `ValidationFailed` (always Permanent — see W4) with a clear
message. The worker is never crashed by user-side bugs.

17 pytest tests in `crates/tars-py/python/tests/test_validators.py`
exercise the eight smoke scenarios (outcome class introspection,
pass-through, reject, filter chain, buggy validator handling, etc.)
plus construction-error guards (validators must be tuples, tuples
must be 2-element).

### B-20 Wave 4 — Cache × Validator interaction fix (A2 path) (`<unreleased>`)

Followup to W1. Two bugs surfaced when downstream consumer (2026-05-08) dogfood prep
flagged a "single-validator-chain assumption" concern; tars-side
audit + failing regression tests confirmed both:

1. `ValidationMiddleware` re-emitted post-Filter events on the
   stream when a Filter outcome rewrote the response. The W1 onion
   put Validation *inside* Cache, so Cache saw and stored the
   post-Filter version — directly contradicting Doc 15 §2's
   "cache stores raw" contract.
2. Cache hits short-circuited before reaching Validation, so the
   "validators rerun on hit" half of the contract was also broken.
   This wasn't fixable by changing re-emit logic alone — fundamental
   onion-order issue.

**Fix — A2 path**: chosen after consulting the consumer on real consumer
needs (zero use cases for `Reject(retriable=True)` model resample;
all validators are deterministic Filter chains).

- **Onion order**: `Telemetry → Validation → Cache → Retry → Provider`.
  Validation now sits OUTSIDE Cache. Cache sees raw Provider events
  (Validation's filtered re-emit is downstream of where Cache reads).
  Cache hits flow through Validation on the way back to the caller —
  validators rerun on every call, hit or miss.
- **`Reject{retriable: bool}` removed**: cut from
  `ValidationOutcome::Reject`, `ProviderError::ValidationFailed`,
  and `tars.Reject(...)`. `ValidationFailed` is always
  `ErrorClass::Permanent`. RetryMiddleware never retries on
  validation failures. Callers needing model resample do so at
  their own layer with explicit prompt variation. Cuts surface,
  removes the temptation to relitigate same-prompt model retries.
- **`PipelineBuilder` callers updated** (`tars-py/src/lib.rs::Pipeline::from_provider`).
  `RetryMiddleware` arm for `validation_failed` retained for
  diagnostics (the kind still surfaces via `TarsProviderError.kind`)
  but never matches the Retriable class predicate.
- **Built-in validator builders**: dropped `JsonShapeValidator::with_retriable`
  and `NotEmptyValidator::with_retriable`; `MaxLengthValidator::OnExceed::Reject`
  no longer carries a `retriable` field.

Two regression tests in `tars-pipeline/src/validation/tests.rs` pin
the new contract (no longer `#[ignore]`'d):

- `b20_w4_cache_stores_raw_not_post_filter` — direct cache registry
  inspection: with Validation OUTSIDE Cache, cached `response.text`
  is the raw provider output, not the filtered version.
- `b20_w4_cache_hit_reruns_validator_chain` — second call's
  `telemetry.layers` includes `"validation"`, proving the chain
  ran on hit.

### B-20.W3 enabler — pipeline event store + body store + EventEmitter (`<unreleased>`)

Phase 1 of [Doc 17](./docs/architecture/17-pipeline-event-store.md) — durable
substrate the W3 main body needs. After this commit every
`Pipeline.complete()` configured with `event_store_dir` lands one
`LlmCallFinished` row per call into a SQLite event store; request +
response bodies go to a separate tenant-scoped CAS body store.

- **`tars-types::PipelineEvent`** — `LlmCallFinished` /
  `EvaluationScored` / `Other` catchall variants. `#[non_exhaustive]`
  + `#[serde(default)]` on new fields + `Other(serde_json::Value)`
  catchall = three layers of forward-compat. Inline scalars
  (model / provider / fingerprint / has_tools / temperature /
  telemetry / validation_summary / tags / result) make 99% of
  cohort-rollup queries hit the event row only; bodies live behind
  `request_ref` / `response_ref` `ContentRef` pointers.
- **`tars-types::ContentRef`** — opaque, tenant-scoped body handle.
  `ContentRef::from_body(tenant, bytes)` computes sha256(bytes) and
  stores it alongside `tenant_id`. **Cross-tenant body dedup
  forbidden** per Doc 06 §1; same body bytes from two tenants get
  two distinct refs (different store keys). Within-tenant dedup
  still happens — most savings come from re-asking the same prompt
  within a tenant anyway.
- **`tars-storage::PipelineEventStore`** — new trait, distinct from
  the existing `EventStore` (trajectory). Q1 in Doc 17 chose two
  independent traits over a generic `EventStore<E>` because access
  patterns are too divergent (trajectory is keyed by
  `TrajectoryId`, pipeline events by tenant + time + tags). Methods:
  `append` / `query` (filter by tenant + time range) / `purge_before`
  / `purge_tenant`. `subscribe()` deferred to Phase 2.
- **`tars-storage::BodyStore`** — new trait + `SqliteBodyStore`
  impl. `INSERT OR IGNORE` makes `put` idempotent CAS. `purge_*`
  ops are first-class trait methods so v2 backends (codex-style
  date-partitioned sqlite-per-day, S3 + lifecycle rules, postgres
  bytea) can implement retention as physical operations rather than
  full-table scans (Doc 17 Q9).
- **`tars-pipeline::EventEmitterMiddleware`** — outermost layer
  (added FIRST to the builder, wraps everything else). Drains the
  stream, builds `ChatResponse`, fires the event + bodies in a
  `tokio::spawn`'d task. Caller's response path doesn't block on
  storage I/O. Failure path emits `LlmCallFinished{result:
  Error{kind}}` with `response_ref: None`. Q2 in Doc 17 chose this
  as a separate layer rather than folding into Telemetry.
- **`tars-py`** — `Pipeline.from_default(..., event_store_dir="...")`
  / `from_config` / `from_str` all accept the new kwarg. Opens both
  `pipeline_events.db` + `bodies.db` under the dir; creates the dir
  if missing. Omitting the kwarg = no event store wired, backwards-
  compatible with existing callers.
- **`Pipeline.complete(..., tags=[...])`** + `RequestContext::with_tags(...)` —
  free-form cohort tags propagate from caller to
  `PipelineEvent.tags`. Cohort SQL: `WHERE 'dogfood_2026_05_08' =
  ANY(tags)`. .
- **`tars events` CLI** — `tars events list [--tenant X] [--since 1d]
  [--tag T] [--limit N]` for one-line-per-event summaries; `tars
  events show <event_id> [--with-bodies]` for full payload + body
  fetch. Diagnostic tool; replaces hand-rolling `sqlite3` queries
  during dogfood prep.
- **`PersistenceMode { Limited, Extended }`** — defined in
  `tars-types`; trait point in place but actual mode-switching logic
  is Phase 2. Default `Limited` (current behaviour). Codex-rs

- **s in this commit**: `Other` catchall variant
  pattern (5 years of versionless schema evolution rest on it),
  `EventPersistenceMode::{Limited, Extended}` dial,
  date-partitioned-storage trait shape (purge_before / purge_tenant
  in the trait so backends can do `rmdir` not full-DELETE).
- **What's deliberately NOT in Phase 1**: `EventStore::subscribe()`
  (live consumer for OnlineEvaluatorRunner), the runner itself,
  per-tag rate sampling, `OnDimDrop` watchdog. All move to Phase 2
  or — based on downstream consumer (2026-05-08) dogfood-prep feedback — get
  re-evaluated against "is this just batch SQL + cron?" before being
  built.

Tests: 21 new cargo tests across content_ref / pipeline_events /
body_store / pipeline_event_store / event_emitter; 7 new pytest
covering event-store integration + tags propagation + dir-creation
edge cases. cargo + clippy clean (`-D warnings`).

Driven by: downstream consumer 2026-05-08 W3 main-body prep; tags surface
specifically promoted from "nice-to-have" to "today blocker" once
Consumer reported they were running cloud + local + smoke batches into
the same `pipeline_events.db` and using `provider_id` + timestamp as
a coarse cohort proxy.

### B-20 v3 — Python `Response.validation_summary` getter (`<unreleased>`)

Rust `ChatResponse.validation_summary: ValidationSummary` was already
populated by `ValidationMiddleware` since W1; this exposes it on the
Python `Response` pyclass so callers can pull per-validator outcomes
for dogfood reports + regression gating.

- **`tars.ValidationSummary`** — new frozen pyclass with fields:
  `validators_run: list[str]` (registration-order chain shape),
  `outcomes: dict[str, dict]` (keyed by validator name, with
  `{"outcome": "pass"|"filter"|"annotate", "dropped"?: list[str],
  "metrics"?: dict}`), `total_wall_ms: int`. `Reject` outcomes are
  absent — they short-circuit into `TarsProviderError`.
- Empty when no validators ran (caller didn't pass `validators=` or
  passed an empty list).

3 new pytest tests cover Filter dropped-list propagation, the
no-validators empty case, and class export.

> Driven by the consumer's dogfood report regression gate: without a
> structured `validation_summary` Python callers had no
> cross-run-comparable view of "the snippet validator dropped 3 of 19
> findings". `r.telemetry.layers` told them *whether* validation
> ran, not *what* it did.

### Stage 4 — `Response.telemetry` per-call observability (`<unreleased>`)

B-15 in TODO. Adds a `.telemetry` field on every `Response` carrying
the operational data callers need for log aggregation, without
forcing them through tracing-subscriber installation gymnastics.

- **New types in `tars-types`**: `TelemetryAccumulator` (cache_hit /
  retry_count / retry_attempts / provider_latency_ms /
  pipeline_total_ms / layers), `RetryAttempt` (error_kind,
  retry_after_ms), `SharedTelemetry = Arc<Mutex<...>>` alias,
  `new_shared_telemetry()` factory.
- **`RequestContext.telemetry: SharedTelemetry`** — every middleware
  along the chain holds an Arc clone, writes its observation through
  the Mutex. Caller pre-creates the handle, keeps an Arc clone
  outside the chain, reads it back after stream drain.
- **Middleware writers**:
  - `TelemetryMiddleware` (outermost): pushes layer name "telemetry";
    stream-end finalize sets `pipeline_total_ms` (success and
    error paths both stamp).
  - `CacheLookupMiddleware`: pushes "cache_lookup"; on hit sets
    `cache_hit=true`. Distinct from `Usage.cached_input_tokens`
    (which is the *provider's* prompt cache) — this field is "we
    avoided the provider call entirely".
  - `RetryMiddleware`: pushes "retry"; per failed-and-retried attempt
    appends a `RetryAttempt { error_kind, retry_after_ms }` and
    bumps `retry_count`. Final-attempt failures don't append (they
    return Err to the caller, no retry).
  - `ProviderService` (innermost): pushes "provider"; wraps stream
    to time HTTP+SSE wall; accumulates into `provider_latency_ms`
    so multi-attempt retry totals reflect total provider time.
- **Python surface** (`tars-py`):
  - `Response.telemetry: Telemetry` — frozen pyclass with all fields.
  - `Telemetry.retry_attempts: list[RetryAttempt]` — `RetryAttempt`
    has `kind: str` (snake-case matching `TarsProviderError.kind`)
    and `retry_after_ms: int | None`.
  - `Session.send` returns `(ChatResponse, TelemetryAccumulator)`
    internally; tars-py's `Session.send` packages both into
    `Response` so callers see telemetry on Session calls too.
    Telemetry across the auto-loop's multiple model calls is
    aggregated under one shared handle (one `Telemetry` per
    `send`, not per model call).
- **Smoke verified**: layers populate outermost-first
  (`['telemetry', 'cache_lookup', 'retry', 'provider']`);
  `provider_latency_ms ≤ pipeline_total_ms`; Session integration
  works; failure paths surface via `TarsProviderError` (no Response
  to attach telemetry — by design).

### `response_schema` kwarg — constrained decoding (`<unreleased>`)

`Pipeline.complete()` / `Provider.complete()` accept
`response_schema=<dict>` triggering provider-side constrained
decoding. Cuts JSON-malformed-output failures at the source
rather than papering over them with lenient parsers (jsonrepair
etc. — explicitly NOT adopted; codex follows the same path).

- Adapter wiring already existed in tars-provider — OpenAI →
  `response_format={type:"json_schema",strict:true}`, Anthropic →
  forced tool_use emulation, Gemini → `response_schema`. Stage-1-of-
  Doc-tier 1 work was just exposing the kwarg in the Python entry.
- `ChatRequest::structured_output: Option<JsonSchema>` is the
  already-existing field; tars-py converts the Python dict via
  JSON round-trip and wraps in `JsonSchema::strict("Response", v)`.
- Smoke (LM Studio + Qwen3-Coder-30B): 3/3 valid JSON with schema,
  0/3 directly parseable without (model wraps in ` ```json...``` `
  markdown by default).

---

## M3 — Agent Runtime (DONE 2026-05-03)

Doc 14 §9 deliverable. **Fully done.** Storage primitive, runtime
facade, agent contract, typed inter-agent envelope, all three default
agents (Orchestrator + WorkerAgent + Critic), the multi-step
`run_task` orchestration loop with Critic-driven Refine retries, the
`tars run-task` CLI, **and** real tool-using WorkerAgent (built on
the new `tars-tools` crate) are all live. WorkerAgent now ships in
two flavours behind the same `Agent`-trait surface: stub (no tools,
LLM describes work) and tool-using (LLM dispatches tool calls
mid-conversation). The same `AgentMessage::PartialResult` envelope
flows out either way, so the orchestration loop is unchanged.

Carried forward to follow-on work (see TODO B-4): replan-on-Reject,
ContextStore + ContextCompactor, PromptBuilder, Backtrack + Saga,
`tars trajectory replay`. None of these block the M3 acceptance
criteria; they're enhancements to a fully functional baseline.

### Audit pin: `system_prompt_hash` in `LlmCallCaptured` (`8b60ecc`)

L-1 enterprise follow-on. Given the trajectory log alone, an
external auditor can now independently verify "this LLM call used
SHA256(...) as its system prompt", then match that hash against
the prompt source shipped in the binary (e.g. `sha256sum read_file.txt`
at the relevant git revision).

- New field: `AgentEvent::LlmCallCaptured::system_prompt_hash:
  Option<String>` (`#[serde(default)]` for migration safety — old
  rows without the field deserialise as `None`).
- New helper: `tars_runtime::event::hash_system_prompt(Option<&str>)
  -> Option<String>`. Plain SHA256 hex of the bytes, no version
  prefix → trivial external verification.
- `execute_agent_step` snapshots the hash from `req.system` before
  the request moves into `agent.execute`.
- `None` means no system prompt was sent; distinct from
  `Some(sha256(""))` (system prompt present but empty) — different
  audit fact.
- Scope: hashes ONLY the system prompt, not the full request
  fingerprint (tools / structured_output / user turns). System
  prompt is the highest-value audit target — it's the model's
  standing instructions.
- Coverage: `tars run-task` (multi-step trajectories) pins every
  LLM call. `tars run` (single-call path) leaves the field `None`
  — threading the system prompt through `TrajectoryLogger` is a
  separate small refactor.
- 8 new unit tests pin determinism, format (64-char lowercase hex),
  none-vs-empty distinction, external SHA256 match, serde
  round-trip, and migration safety for old payloads.

### Tool prompt assets + ToolResult.title + Retry-After parsing: L-1 + L-3 + L-4 (`7290e27` / `c5d8e5d`)

Three `defer > delete > implement` items from this commit
survey (TODO L-1..L-12). All three were "do now" tier — small
cost, immediate value, no dependencies.

**L-1: tool descriptions externalized to `.txt` files** (`7290e27`)
- `Tool::description()` returns `include_str!("read_file.txt").trim_end()`
  via a `LazyLock<String>` instead of an inline `&'static str`.
- New sibling files: `crates/tars-tools/src/builtins/read_file.txt`
  + `list_dir.txt`. Tool prompt assets stored as `*.txt` next to the tool source.- **Wins**: prompt diffs review separately from Rust changes; clean
  per-prompt git history; future i18n via per-locale `.txt` swap.
- **Security posture**: `include_str!` is a compile-time embed — the
  prompts are baked into the signed binary, no runtime mutation
  surface, no per-tenant cross-contamination via shared filesystem.
  This is the right enterprise posture; runtime file loading would
  be a real prompt-injection escalation surface and is deliberately
  NOT done. (Earlier "no recompile needed" framing was incorrect —
  editing a .txt does still require `cargo build`.)

**L-3: `ToolResult.title` for trajectory + future-TUI readability** (`7290e27`)
- `ToolResult { title: String, content: String, is_error: bool }`
  with new constructors `titled_success` / `titled_error` alongside
  the untitled ones (back-compat for external callers).
- `ReadFileTool` fills `"Read foo.rs (4096 bytes)"` /
  `"foo.rs not found"` / `"foo.rs is not UTF-8"`.
- `ListDirTool` fills `"Listed src/ (23 entries)"` /
  `"Listed src/ (256+ entries, truncated)"`.
- `ToolRegistry::dispatch` emits a `tracing::info!` with the title;
  the title is **not** placed into `Message::Tool` (LLM-visible
  content stays unchanged).

**L-4: Retry-After header parsing in `RetryMiddleware`** (`c5d8e5d`)
- New `tars_provider::http_base::parse_retry_after(&HeaderMap) ->
  Option<Duration>` with three-tier resolution:
  1. `retry-after-ms` (millisecond-precision; Anthropic uses this)
  2. `retry-after` as positive integer (seconds, RFC 7231)
  3. `retry-after` as HTTP date (past dates clamp to ZERO so the
     caller can retry immediately)
- API change: `HttpAdapter::classify_error` grew a `&HeaderMap`
  parameter between `status` and `body`. http_base.rs snapshots
  headers before consuming the response.
- All three HTTP backends (openai / anthropic / gemini) now populate
  `RateLimited::retry_after` from headers instead of `None`.
- `RetryMiddleware` already had `respect_retry_after = true` by
  default — now it actually has a value to honor.
- Dep added: `httpdate 1` (~5 KB compiled, RFC 7231 date parser).
- 7 unit tests on the helper + 2 backend tests pinning the populated
  field. tars-provider: 99 tests (was 90).

### `codex_cli` provider + `tars probe` (`a4e2254` / `72091b4` / `dd99b48` / `8712937`)

Third subscription-CLI provider lands alongside `claude_cli` /
`gemini_cli` — gives TARS users a way to leech ChatGPT Plus/Pro
inference for `gpt-5.5` / `gpt-5.4` / `gpt-5.3-codex` etc. without
burning API credits. All three subscription-leech paths verified
end-to-end against real binaries.

- **`codex_cli` backend** (`a4e2254`) — `tars-provider/src/backends/
  codex_cli.rs`. Spawns `codex exec --json --model X --sandbox
  read-only -c approval_policy="never" --skip-git-repo-check -`,
  feeds prompt on stdin, streams stdout JSONL line-by-line. Strips
  `OPENAI_API_KEY` / `CODEX_API_KEY` / `CODEX_AGENT_IDENTITY` env
  vars (case-insensitive) to force codex through `~/.codex/auth.json`
  (ChatGPT OAuth) instead of the API path. Same posture as
  `claude_cli`'s `ANTHROPIC_API_KEY` strip.

- **ThreadEvent → ChatEvent mapping (v1, conservative)**: drops
  codex's internal scratch work, surfaces only the LLM's text:

      agent_message.text  → Delta { text }
      reasoning.text      → ThinkingDelta { text }
      turn.completed      → Finished { EndTurn, <converted usage> }
      turn.failed         → ProviderError::CliSubprocessDied
      lifecycle / tool / file_change / mcp_tool_call / web_search
                           / todo_list events → drop in v1
      unknown variants    → log + skip (forward-compat)

  Folding tool/file events into Delta text is a v2 knob. Today's
  consumer (TARS `WorkerAgent` + Critic) only cares about the final
  text answer, and surfacing codex's internal sandbox-shell
  invocations would pollute summaries + confuse the Critic.

- **Real-binary fixes** (`72091b4`) — first smoke run caught two
  bugs the docs lied about:
  - `--ask-for-approval` flag doesn't exist in codex 0.128 — the
    documented flag is gone in favor of `-c approval_policy="never"`
    config override.
  - Bare model names `gpt-5` / `gpt-5-codex` are API-only; ChatGPT
    accounts must use tier-specific names like `gpt-5.5` / `gpt-5.4`
    / `gpt-5.3-codex`. Backend doesn't hardcode anything — caller
    picks per their account.

- **Live smoke tests for all three subscription CLIs** (`72091b4` /
  `dd99b48`) — `#[ignore]`-d so normal `cargo test` doesn't burn
  quota. Run with `cargo test -p tars-provider --test
  <claude|gemini|codex>_cli_smoke -- --ignored --nocapture`.
  Every backend produces a clean Started → Delta → Finished triple
  with usage (input/output/cached/thinking) all populated:

      claude_cli  sonnet           "hello from claude"
                    in=3 out=6 cached=11631 thinking=0
      gemini_cli  gemini-2.5-pro   "hello from gemini"
                    in=5804 out=4 cached=0 thinking=17
      codex_cli   gpt-5.5          "hello from codex"
                    in=13023 out=19 cached=11648 thinking=9

- **`tars probe <cli-provider>`** (`8712937`) — exposes the smoke
  pattern as a user-facing subcommand. Loads config, validates the
  named provider is `*_cli` type (HTTP types get a friendly hint to
  use `tars run` instead), sends a fixed "say hi" prompt, streams
  every ChatEvent to stderr in human-readable form. Useful for
  debugging "why doesn't `tars run-task -P codex_cli` work" —
  shows exactly which step (auth / binary / mapping) broke.

  Args: `--model <NAME>` (override default for tier-restricted
  accounts), `--prompt <STR>` (custom prompt).

### Audit fixes — round 4 (`57c893d`)

A.R.C. run `1d8e3308` against `148cda5`. Ergonomics + actionability
fixes across 7 files:

- **tars-cache/error.rs**: explicit test that `CacheError::Serialize`
  is NOT classified as `is_not_cacheable` (a refactor of the helper
  could silently misclassify it without coverage).
- **tars-config/error.rs + manager.rs**: `ValidationError` gets the
  `thiserror::Error` derive (cleaner Display via `#[error("...")]`,
  drops the hand-rolled fmt impl). New `ConfigError::validation_failed`
  ctor with `debug_assert` against an empty error list — prevents
  the "config validation failed (0)" footgun.
- **tars-config/builtin.rs**: better panic messages in tests
  (expect/got context instead of bare `panic!`); 
  arms to avoid moves before formatted-panic branches.
- **tars-provider/auth.rs**: empty/whitespace env vars + empty
  credential files now surface as `AuthError::Missing` instead of
  becoming a mysterious downstream 401.
- **tars-types/context.rs**: split `is_deadline_exceeded` out from
  `is_cancelled`, but `is_cancelled` now ORs with it so deadline
  expiration cancels in-flight requests cleanly. Callers wanting to
  distinguish caller-cancel from hard timeout get the separate
  accessor.
- **tars-pipeline/cache.rs**: cache-write failure log now includes
  the cache key's `debug_label` so warnings are actionable.

### `PromptBuilder` extraction (`8fdeed1`)

By the end of M3 four agents (Orchestrator + Critic + WorkerStub +
WorkerTools) had hand-rolled the same six lines: `ChatRequest::user`
+ system prompt + structured_output + `temperature=0.0` + optional
`tools`. Trigger-4 reached → extracted to `PromptBuilder` (fluent
recipe builder), 7 unit tests + all 60+ existing agent tests still
green.

What this is **not**: Doc 04 §6's full block-composition
PromptBuilder (persona + role + tool-doc + format-rules as separate
typed blocks). No agent today has multi-source prompts; the block
variant slots in once a second persona ships (probably alongside
multi-tenant work in M6).

### WorkerAgent + tools — stub becomes real (`148cda5`)

Wires the new `tars-tools` crate into WorkerAgent. The stub still
exists (`WorkerAgent::new`); `WorkerAgent::with_tools(..., registry)`
adds the tool-using flavour.

- **Inner dispatch loop** lives in `WorkerAgent::execute` itself,
  NOT in `run_task` — one `Agent::execute` call drives N internal
  LLM calls (drain stream → on tool calls dispatch via registry →
  append assistant + tool messages → re-prompt → repeat). Stops on
  first text-only answer or `max_tool_iterations` (default 8).
  Usage sums across calls.
- **Trajectory observability tradeoff**: the loop is invisible to
  the trajectory layer (one StepStarted/LlmCallCaptured/StepCompleted
  per Worker step regardless of tool round-trip count). Deferred
  until per-call replay has a consumer — the new event variants
  would be `LlmSubcallCaptured` + `ToolCallExecuted`, slotting in
  alongside Backtrack + Saga.
- **Two system prompts**: `WORKER_SYSTEM_PROMPT` (no tools) +
  `WORKER_SYSTEM_PROMPT_WITH_TOOLS` (instructs "call tools when you
  need them, only emit final JSON when done"). `structured_output`
  stays set in both — strict mode + tool calls coexist; tool calls
  bypass the response_format constraint, only the final text-only
  answer must conform.
- 4 integration tests in `tests/worker_with_tools.rs` using a small
  `EventQueueProvider` that pops `Vec<ChatEvent>` per call. Cover
  real `fs.read_file` dispatch + result threading, tool-spec
  advertising on first call, max-iteration safety cap, and
  stub-flavour regression.

### `tars-tools` crate — Tool trait + ToolRegistry + fs.read_file (`c4c5357`)

10th workspace member. The executable side of tool calling — typed
plumbing (`ToolSpec` / `ToolCall` / `Message::Tool`) already lived in
`tars-types`; this crate adds what actually runs.

- **`Tool` trait** — async `name() / description() / input_schema() /
  execute(args, ctx) -> Result<ToolResult, ToolError>`. Same
  `Arc<dyn Tool>` handle pattern as `Arc<dyn Agent>` in
  `tars-runtime`.
- **`ToolContext`** — `cancel` + `cwd` today; principal/tenant/
  deadline/budget slot in as their backing crates ship (matches
  `AgentContext` rationale).
- **`ToolResult { content, is_error }`** distinct from `ToolError`:
  Result is "tool ran but the operation failed, LLM should adapt";
  Error is "couldn't even attempt — Cancelled / InvalidArguments /
  Execute".
- **`ToolRegistry`** — name-keyed lookup, `register` errors on
  duplicate (silent overwrite would be a footgun), `to_tool_specs()`
  for `ChatRequest.tools`, `dispatch(call) → Message::Tool`. Both
  lookup-miss and execute-error become `is_error=true` messages
  rather than `Result::Err` so the agent loop has something to feed
  the model on the next turn.
- **Built-in `fs.read_file`** — UTF-8 read with optional path-jail
  (canonicalize-then-starts_with), 256 KiB default cap,
  NotFound/Binary/TooLarge surface as clean `is_error` results.
  Cancel-aware. The first real Tool — exercises every trait
  responsibility end-to-end so additional read-only tools become
  mechanical to write.
- **Out of scope**, each gets its own commit when consumer appears:
  idempotency tags (today's `StepIdempotencyKey` covers per-step
  dedupe), side-effect declarations (need Saga from Doc 04 §6),
  iam_scopes (need `tars-security` M6), budget_hint (need
  BudgetMiddleware), timeout (CancellationToken covers
  upstream-cancel today). Additional builtins (`fs.write_file`,
  `fs.list_dir`, `git.*`, `web.fetch`, `shell.exec`) ship one at a
  time as WorkerAgent needs them — `fs.write_file` specifically
  waits for Saga thinking before it can ship safely.
- 19 unit tests covering trait basics, registry register/get/names/
  dispatch, ReadFileTool happy + jail + size cap + binary + cancel +
  invalid args + missing file paths.

### `fs.list_dir` builtin + CLI wire-up (`b4cc406`)

Second built-in tool. Pairs with `fs.read_file`: the LLM can't read
what it hasn't located, so adding `fs.list_dir` lets prompts like
"summarise the README in this repo" work without prompts containing
literal paths.

- Same safety posture as `fs.read_file`: optional path-jail, cancel-
  aware, 256-entry default cap (truncation flagged in output so the
  LLM knows to try a more specific path).
- Output format: one entry per line, sorted, with type glyph (`d`
  dir / `f` file / `l` symlink) + name + optional size or symlink
  target. Compact for the LLM context, structured enough that the
  LLM doesn't have to guess what's a directory.
- Edge cases (not found, not-a-directory, outside jail root) surface
  as `is_error=true` ToolResult — the not-a-directory path also
  hints "use fs.read_file instead" so the model self-corrects.
- Wired into `tars run-task --tools` alongside `fs.read_file`,
  sharing the same jail root.
- 10 unit tests; tars-tools now has 29 unit tests total.

### `tars run-task --tools` flag (`87845aa`)

Capstone on M3. Wires `fs.read_file` (jailed to cwd by default) into
the CLI's WorkerAgent so `tars run-task -g "summarise the README in
this repo" --tools` actually drives a real tool-using triad — no
Rust call-site needed.

- `--tools` enables the default safe set (today: `fs.read_file` only;
  read-only ones like `fs.list_dir` / `git.fetch_pr_diff` /
  `web.fetch` will join as they ship).
- `--tools-root <PATH>` overrides the jail root (default: process cwd).
- Side-effecting tools (`fs.write_file`, `shell.exec`) won't join the
  default set — they'll get explicit opt-in flags so the safe baseline
  stays safe.
- Stderr prints the enabled tool list + jail root before any prompt
  fires.

### `tars run-task <goal>` CLI subcommand (`959be20`)

Wires `tars_runtime::run_task` into the CLI alongside `tars run` and
`tars plan`. The user-facing M3 entry point — humans can now drive
the full Orchestrator → Worker → Critic loop from a single command
instead of needing Rust call-site access.

- Shares the same `DispatchArgs` (provider/tier/model/cache/breaker/
  trajectory) as `tars run` / `tars plan` so flag semantics stay
  uniform.
- Specific args: `--goal/-g`, `--max-refinements N` (default 2 —
  matches `RunTaskConfig::default`), `--worker-domain LABEL`
  (default `general`; surfaces in `AgentRole::Worker`), `--json`
  (full `TaskOutcome` as JSON instead of human format).
- Trajectory id printed to stderr **always** — including on failure
  paths — so the user can immediately `tars trajectory show <ID>`
  to inspect what happened.
- `--no-trajectory` falls through to an in-memory `SqliteEventStore`
  rather than disabling the runtime (run_task requires a real
  `Runtime`); events still flow but leave no SQLite artefacts.

### `run_task` multi-step loop + WorkerAgent stub (`c3cea5b`)

The first user-facing M3 piece. Given a goal, drives the typed agent
triad end-to-end with full trajectory logging. The actual M3
milestone Doc 14 §9 specifies.

- **`WorkerAgent`** (`worker.rs`) — third concrete default agent.
  Stub today (no tool registry until B-9 ships); emits the typed
  `AgentMessage::PartialResult` envelope a real tool-using Worker
  will produce later, so downstream code (Critic, orchestration
  loop, replay) doesn't need to change when the stub becomes real.
  Same flat-JSON-schema-on-the-wire pattern as Critic: model emits
  `{summary, confidence}`, we map to typed `PartialResult` with
  confidence clamped to `0.0..=1.0`. `temperature=0.0` baked in for
  determinism.
- **`run_task`** (`task.rs`) — the loop itself. Orchestrator → Plan
  → for each step: Worker → Critic → (Approve advance / Refine retry
  with suggestions threaded into next Worker prompt / Reject fail).
  Bounded by `RunTaskConfig::max_refinements_per_step` (default 2).
  Every agent call routes through `execute_agent_step` so the
  trajectory log captures `StepStarted + LlmCallCaptured +
  StepCompleted` per call (or `StepFailed` on error). Trajectory
  closes with `TrajectoryCompleted` on success or
  `TrajectoryAbandoned` on any failure path so a recovery scan sees
  it as terminal. Replan-on-Reject deferred — first cut treats
  Reject as task-failed.
- **Refactor**: `OrchestratorAgent::build_planner_request` +
  `parse_plan_response` and `CriticAgent::build_critique_request`
  + new `parse_verdict_response` are now `pub` (were `pub(crate)`)
  so `run_task` can split build/execute/parse across
  `execute_agent_step`. The typed `plan()` / `critique()` helpers
  still exist for callers that want one-shot use.
- **6 integration tests** (`tests/run_task.rs`) using a local
  `QueuedProvider` that pops canned JSON in FIFO order — happy
  path single-step, refine-then-approve with suggestion threading,
  reject aborts + abandons trajectory, refine exhaustion with
  attempt count, multi-step plan ordering, malformed plan abandons.
  Per-test trajectory event-count assertions pin the
  `1 + 3*N + 1` shape so future changes that miss an event boundary
  fail loudly.

### Agent ecosystem additions (2026-05-03 follow-up wave)

- **`OrchestratorAgent` + `Plan`/`PlanStep` types** (`09546bd`) — first
  concrete LLM-driven planner. `OrchestratorAgent::plan(goal)` typed
  helper builds a strict-JSON-schema-enforced ChatRequest
  (system + temperature=0 + Plan schema), runs through the LLM,
  parses + validates the dependency graph. Linear plans for MVP
  (`depends_on` field reserved for parallel-fan-out work later).
  9 unit + 4 integration tests.
- **`tars plan <goal>` CLI subcommand + `dispatch` module refactor**
  (`89dba0f`) — wires Orchestrator into the CLI. Pretty/compact JSON
  output, full trajectory logging via `execute_agent_step`. Same
  refactor extracted ~200 lines of dispatch / cache / registry /
  pick_provider plumbing from `run.rs` into a shared
  `tars-cli/src/dispatch.rs` module so future subcommands (`tars chat`)
  flatten the same `DispatchArgs` and can't drift on flag semantics.
- **`AgentMessage` typed inter-agent envelope** (`5d0d2a5`) — Doc 04
  §4.2's "禁止纯文本互喷" piece. 4 variants chosen for concrete
  near-term consumers: `PlanIssued{plan}`,
  `PartialResult{from_agent, step_id, summary, confidence}`,
  `Verdict{from_agent, target_step_id, verdict: VerdictKind}`,
  `NeedsClarification{from_agent, question}`. `VerdictKind` =
  `Approve` / `Reject{reason}` / `Refine{suggestions}`. No
  `#[serde(other)]` catchall — unknown variants fail loudly.
  Tag names pinned per-variant by test. 9 unit tests.
- **`CriticAgent`** (`02ac233`) — second concrete default agent.
  `critique(plan, partial_result, goal)` returns a typed
  `AgentMessage::Verdict` envelope. Flat JSON schema on the wire
  (`{kind, reason, suggestions}` all required) avoids `oneOf`
  gymnastics that OpenAI strict mode handles awkwardly; mapped to
  typed `VerdictKind` in the typed helper. `PartialResultRef<'a>`
  view-borrow so the owned message stays available for the
  trajectory log. `CriticError` separates Decode failure from
  semantically-broken-but-parseable (`InvalidVerdict` — e.g. `kind=reject`
  with empty `reason`). Critic system prompt biases toward
  actionable feedback ("when uncertain between approve and refine,
  prefer refine"). 11 unit + 6 integration tests.

### Audit fixes — round 3 (`af2d8f1`)

A.R.C. run `71d49588` against `09546bd`. 76 findings: 5 errors +
9 info + 62 warnings.

- **registry-1**: `MemoryCacheRegistry::write` was lying about
  per-write `l1_ttl` — the constructor doc claimed override, but
  `let _ = ...` discarded the value silently (moka's `Cache::insert`
  doesn't take per-entry TTL without an `Expiry` policy on the
  builder). Now logs at debug when caller passes a non-default
  `l1_ttl` so the gap is visible. `SqliteCacheRegistry`'s L2 path
  always honored it.
- **trajectory-1**: `tars trajectory list` bailed out on the first
  per-row replay() failure, hiding all other (working) trajectories
  from the user. Per-row failures now render as `<error>` with cause
  logged via `tracing::warn`; the listing continues.
- **cache-1 / cache-5**: `RwLock` poison cases in `read_policy` /
  `set_cache_policy` were silent. Now log at `tracing::error`
  before degrading to default — poisoned-lock incidents leave a
  trace.
- **context-1**: re-disputed (duplicate of round-2's same finding;
  tracked as A-6 in TODO with M6 trigger).
- 9 info + 62 warnings bulk-ignored as TODO A-1's "test-quality
  pass" bucket.

### tars-storage — EventStore + SqliteEventStore (`e348c09`)
- 8th workspace member. Trait + SQLite impl that backs trajectory
  replay (Doc 04 §3) and is the durability primitive
  recovery-from-checkpoint relies on.
- `EventStore` trait (5 methods: append / read_all / read_since /
  high_water / list_trajectories). Boundary type is
  `serde_json::Value` not generic `<E>` — keeps the trait
  monomorphic so `Arc<dyn EventStore>` works cleanly + makes rows
  debuggable via `sqlite3 events.db`.
- `SqliteEventStore` reuses the `tars-cache::SqliteCacheRegistry`
  scaffolding pattern (single connection in `Arc<Mutex>`,
  spawn_blocking for ops, WAL + `synchronous=NORMAL` +
  `temp_store=MEMORY`, `user_version` migration marker). Key
  departure from the cache crate's policy: an unknown prior
  schema_version **refuses to migrate** rather than wiping —
  events are durable user history, not cache.
- `(trajectory_id, sequence_no)` composite PK; `sequence_no`
  computed inside an open transaction so per-trajectory writes
  stay gap-free under concurrent calls.
- Default location: `dirs::data_dir()/tars/events.sqlite` (XDG
  data dir, NOT cache dir — events ≠ cache).
- 12 tests including `append_survives_close_and_reopen` (the
  recovery promise) and `reopen_with_unknown_schema_version_errors`.
- `ContentStore` + `KVStore` from Doc 14 §6.1 deliberately deferred
  — no consumer yet, per `defer > delete > implement`.

### tars-runtime — AgentEvent + Runtime + LocalRuntime (`7c93e6e`)
- 9th workspace member. Thin facade over `EventStore` that handles
  trajectory creation + typed-event append/read.
- `AgentEvent` enum, 8 variants for the first cut: 4 trajectory-
  lifecycle (Started / Completed / Suspended / Abandoned) + 3
  step-lifecycle (StepStarted / StepCompleted / StepFailed) + 1
  external-call capture (LlmCallCaptured).
- Doc 04 §3.2 has 10 variants including separate
  `LlmResponseCaptured` (raw bytes for parser-rewind replay) +
  `Compensation*` + `Checkpoint`. Skipped — no consumer:
  - Compensation* lands with the Saga work.
  - Llm{Response/Step}Completed split matters when "we changed
    the parser, replay against raw bytes" is real; today we
    record summaries.
  - Checkpoint becomes useful when replay is dominating recovery
    cost; today's trajectories are short.
- `StepIdempotencyKey::compute(traj, step_seq, input_summary)`
  — Doc 04 §3.2 invariant 3. Stored inline on `StepStarted`;
  external operations carry it as metadata for replay dedupe.
  64-char lowercase hex format pinned by test.
- `Runtime` trait + `LocalRuntime` impl. Mints `uuid v4 simple`
  trajectory ids. Defensive guard: `append()` rejects events
  whose embedded trajectory_id doesn't match the append target
  (catches the obvious bug at the runtime layer).
- 15 unit tests + 2 integration tests
  (`tests/recovery.rs::trajectory_survives_runtime_restart`
  proves the recovery-from-checkpoint promise end-to-end).
- Field shapes deliberately primitive (String summaries, plain
  ProviderId/Usage). Doc 04's typed `BranchReason`, `ContentRef`,
  `AgentMessage`, etc. land when their consumers exist.

### tars-cli runtime integration + `tars trajectory` (`4460c3e`)
- Every `tars run` now opens a trajectory and writes the lifecycle:
  `Started → StepStarted → LlmCallCaptured → StepCompleted →
  Completed` (or `StepFailed → Abandoned` on error). Footer
  appears after the summary: `── trajectory: <uuid>`.
- Best-effort discipline: every trajectory write swallows errors
  with `tracing::warn` rather than propagating. SQLite hiccup
  must not block the user's LLM response — same Doc 03 §4.3
  stance the cache uses.
- `--no-trajectory` opt-out + `--events-path` override
  (`TARS_EVENTS_PATH` env). Default location:
  `$XDG_DATA_HOME/tars/events.sqlite`.
- New `tars trajectory` subcommand:
  - `list` — id / event count / status (active / completed /
    abandoned).
  - `show <ID>` — every event as JSON lines on stdout (pipeable:
    `tars trajectory show ID | jq -c …`).
- Deferred subcommands (no consumer): `delete <ID>` (needs
  retention policy), `replay <ID>` (needs the Agent execution
  loop to know what "replay" means at the action level).
- `StreamOutcome` gained `response_text: String` so the trajectory
  log's `output_summary` doesn't re-read the network.
- New shared module `event_store::open()` keeps `tars run` and
  `tars trajectory` from drifting on default-path resolution.

### Agent trait + SingleShotAgent + execute_agent_step (`f6b6c4e`)
- The first M3 agent primitive. Real Orchestrator/Worker/Critic
  agents stack on this once the typed `AgentMessage` protocol +
  prompt design land.
- `Agent` trait: `id() / role() / execute(ctx, input) -> Result<AgentStepResult, AgentError>`.
- `AgentRole` enum: Orchestrator / Worker { domain } / Critic.
  Doc 04 §4.1 also lists `Aggregator` (pure-code agent, no LLM);
  skipped — no consumer.
- `AgentContext` minimal — `trajectory_id + step_seq + llm`
  (`Arc<dyn LlmService>` from tars-pipeline) `+ cancel`. Doc 04 §4.1
  lists more (budget, principal, deadline, context_store,
  tool_registry); each slots in as its backing crate ships.
- `AgentOutput` enum: `Text / ToolCalls / Mixed`. Constructed from
  a drained `ChatResponse`'s (text, tool_calls) via
  `from_response_parts`. `summary(max_chars)` for trajectory log
  payloads (200-char cap today).
- `AgentError`: `Provider(ProviderError) / Cancelled / Internal`.
  `classification()` maps to one-word strings (permanent /
  retriable / maybe_retriable / cancelled / internal) — same
  shape `AgentEvent::StepFailed::classification` expects.
- `SingleShotAgent`: drains an LLM stream → ChatResponse →
  AgentOutput. Cancel-aware: `select!`s `ctx.cancel.cancelled()`
  against both stream-open AND each event poll so a Drop'd parent
  doesn't leak the HTTP/subprocess connection. Role is
  `Worker { domain: "single_shot" }` — placeholder until real
  domain agents land.
- `execute_agent_step()` free function wraps `Agent::execute` with
  full event-log writes (`StepStarted → LlmCallCaptured +
  StepCompleted` or `StepFailed`).
  - **Bug caught + fixed by tests**: `step_seq` was being computed
    as `event_high_water + 1`, off-by-one'ing the very first
    step (TrajectoryStarted occupies event_seq=1). Fixed to count
    `StepStarted` events specifically — `step_seq` is the LOGICAL
    step identifier (Doc 04 §3.2 invariant 3), not the event
    sequencing primitive. The
    `step_seq_increments_across_multiple_agent_calls` test pins it.
  - Storage failures propagate as `RuntimeError` (not best-effort)
    — internal-tool stance, opposite of the CLI's "logging is
    optional, never fatal" stance.
- `AgentExecutionError` splits Agent failure (the model said no)
  from Runtime failure (event store is down).
- `AgentId` joins the existing string-id family in tars-types.
- 6 unit tests in agent.rs + 3 integration tests in
  `tests/agent_step.rs` driving the full stack (Pipeline +
  ProviderService + MockProvider + LocalRuntime + SqliteEventStore
  on disk).

---

## M2 — Multi-provider + Routing (DONE 2026-05-03)

Doc 14 §8 deliverable. Provider impls were already in from earlier
work; M2's defining additions are the **routing + circuit-breaker +
conformance-test triple** that turns 9 backends into a composable
fallback chain.

### Routing (`a4ebba9`)
- `RoutingPolicy` trait + `StaticPolicy` (caller-decides) + `TierPolicy`
  (config-driven `ModelTier → Vec<ProviderId>`) + `RoutingService`
  (bottom-of-pipeline LlmService).
- **FallbackChain inlined** into `RoutingService.call`'s try-each loop —
  returning an ordered `Vec<ProviderId>` from `select()` IS the
  fallback primitive. Dropped the planned `FallbackChain<P>` wrapper:
  composing chains-of-policies is uncommon enough that a list-of-IDs
  is the right shape.
- `tars-config::RoutingConfig` with `[routing.tiers]` TOML section +
  validator that catches dangling `ProviderId` references at startup.
- CLI `--tier <NAME>` flag (mutually exclusive with `--provider`).
- 10 unit tests + integration via the CLI's existing path.

### CircuitBreaker (`caf0043`)
- Per-provider state machine: `Closed → Open → HalfOpen` driven by
  consecutive open-time failures + cooldown. Concurrent HalfOpen
  callers serialise via `probe_in_flight`.
- New typed `ProviderError::CircuitOpen { until }` variant — class =
  `Retriable` so an upstream `RoutingService` falls through to the
  next candidate naturally. `retry_after()` reports remaining cooldown.
- **Design choice**: wrap the Provider, not a separate Middleware
  layer. `CircuitBreaker` impls `LlmProvider` and slots into a
  `ProviderRegistry` slot via `CircuitBreaker::wrap()`. Doc 02 §2
  diagrams it as an onion layer; wrapping the provider is functionally
  equivalent and avoids the contortions of "a middleware that knows
  which provider it's wrapping" given that our `Middleware` trait
  wraps an opaque inner `LlmService`.
- 7 unit tests cover every state transition.

### CLI `--breaker` wiring (`06502a8`)
- New `ProviderRegistry::from_map()` + `map_providers(f)` helpers so
  callers can transform an existing registry without re-running the
  config factory.
- CLI `--breaker` flag (default off) wraps every registry provider in
  `CircuitBreaker` before dispatch. Default-off is deliberate: a
  single `tars run` invocation has no cross-call breaker value;
  Retry already covers within-request retry. The flag exists to demo
  the composition + give long-lived future consumers (REPL / server)
  a reference path.

### Cross-provider conformance suite (`51ae7fc`) — D-9 closed
- `crates/tars-provider/tests/conformance.rs` — `conformance_suite!`
  macro instantiates 6 invariants × 3 HTTP backends = 18 tests:
  streaming text, tool-call args-as-Object, 401→Auth, 429→RateLimited,
  503→ModelOverloaded, capability sanity.
- Adding a 4th HTTP backend is `impl Scenarios` + one
  `conformance_suite!(name, MyScenarios);` line.
- **Caught a real Gemini bug on first run**: `finishReason="STOP"` was
  returning as `EndTurn` even on tool-call responses, while
  OpenAI/Anthropic both normalize to `ToolUse`. Fixed in same commit
  by tracking `had_function_call` per candidate and overriding.

---

## M1 — Single-provider end-to-end (DONE 2026-05-03)

Doc 14 §7 + §11 deliverable. The Doc 14 §7.2 acceptance script
(`tars run --prompt …` streams text + tokens + cost; second identical
call hits cache) works end-to-end.

### tars-pipeline skeleton (`bdaf3b5` + `15600a2`)
- `LlmService` trait — same return shape as `LlmProvider::stream` so
  providers slot in as the innermost service unchanged.
- `Middleware` trait + `Pipeline::builder()` with outermost-first layer
  composition.
- `ProviderService` (bottom adapter), `TelemetryMiddleware` (structured
  tracing on open/fail/finish/mid-stream-error), `RetryMiddleware`
  (open-time only, error-class driven, cancel-aware sleep).
- 10 unit tests + 3 wiremock integration tests.
- **Not** doing mid-stream retry — replaying a partially-consumed
  stream double-emits deltas.

### tars-cache L1 + CacheLookupMiddleware (`aaed3de`)
- `CacheKey` + `CacheKeyFactory` enforcing the Doc 03 §3 security
  shape: hasher_version first, tenant + IAM scopes prefix every hash,
  sorted scope encoding (`{a,b}` == `{b,a}`), `temperature ≠ 0` rejects
  with typed `NonDeterministic` error.
- `MemoryCacheRegistry` (moka W-TinyLFU, 10K entries default,
  5min default TTL).
- `CacheLookupMiddleware` sits between Telemetry and Retry. On hit,
  replays via `ChatResponse::into_events()` — outer middleware can't
  tell the replay from a fresh stream. Per-call `CachePolicy` override
  via `ctx.attributes["cache.policy"]`.
- New `CacheHitInfo.replayed_from_cache: bool` distinguishes a full
  L1/L2 replay from L3-style prefix-discount cache hits.
- 18 cache tests + 6 middleware tests.

### tars-cli (`3cece4f`)
- `tars run --prompt --provider --model --system --max-output-tokens
  --temperature --no-summary -v[vv]`.
- Output discipline: response → stdout (pipeable), tracing → stderr.
- Provider selection rule: explicit `--provider` wins; if exactly one
  provider configured, use it; else error listing candidates.
- Config resolution: `--config` flag → `$TARS_CONFIG` env →
  `dirs::config_dir()/tars/config.toml`.

### tars-cache L2 SQLite (`6079126`)
- `SqliteCacheRegistry` — in-process moka L1 + rusqlite L2 in one
  type. Read-through L1 → L2 → fill L1 on hit. SQLite ops run inside
  `tokio::task::spawn_blocking`.
- WAL + `synchronous=NORMAL` + `temp_store=MEMORY` pragmas. Schema
  versioned via SQLite's `user_version` PRAGMA.
- TTL via `expires_at_ms` per row; lookup filters expired; every 64th
  write fires a best-effort sweep. No background janitor (M3 work).
- CLI `--cache-path` flag (default `dirs::cache_dir()/tars/cache.sqlite`,
  `:memory:` sentinel for tests). Default `CachePolicy` flipped to
  l1+l2 ON now that L2 is real.
- **The Doc 14 §7.2 acceptance script fully passes** — second `tars
  run` invocation reports `(cache hit; cost saved)`.

### tars-melt mini (`080b05f`)
- Shared `tracing` init for all binaries. `TelemetryConfig::from_verbosity(u8)`
  + `TelemetryFormat::{Pretty, Json}` + `TARS_LOG_FORMAT` env knob.
- `TelemetryGuard` placeholder type so future M5 OTel exporter Drop
  hook slots in without breaking callers.
- Replaced the inline init in `tars-cli/src/main.rs`; `tracing-subscriber`
  dep moved out of the CLI.
- **Deferred to M5** (per B-8): metrics, OTel SDK + OTLP exporter,
  cardinality validator, generic `SecretField<T>` (today
  `SecretString` covers the only consumer), trace head + tail
  sampling.

---

## M0 — Foundation (DONE 2026-05-03)

Doc 14 §6 deliverable. Workspace + the type / config / provider /
audit-fix base everything else builds on.

### Workspace + types
- 7-crate Cargo workspace, edition 2024, rust 1.85+.
- `tars-types`: `ChatRequest` / `ChatEvent` / `ToolCall` /
  `Capabilities` / `ModelHint{Explicit,Tier,Ensemble}` /
  `ThinkingMode` / `RequestContext` / `ProviderError` /
  `Auth` / `SecretRef` / `SecretString` / strongly-typed IDs
  (`TenantId`, `SessionId`, `ProviderId`, …).

### tars-config + builtin defaults
- `Config` + `ProvidersConfig` + `ProviderConfig` (8 variants:
  `Openai` / `OpenaiCompat` / `Anthropic` / `Gemini` / `Vllm` /
  `Mlx` / `Llamacpp` / `ClaudeCli` / `GeminiCli` / `Mock`).
- `#[serde(deny_unknown_fields)]` everywhere — typos at the TOML
  layer fail loud.
- `built_in_provider_defaults()` table  + user
  config merging via `merge_builtin_with_user()`.
- `ConfigManager::load_from_file/str()` + structured
  `ValidationError` collection (no fail-fast — operators want the
  full list to fix in one pass).

### tars-provider — 9 backends
- HTTP API: **OpenAI**, **Anthropic**, **Gemini** + 3
  OpenAI-compatible wrappers (**vLLM**, **MLX**, **llama.cpp** — all
  ride the OpenAI adapter with different `base_url` + capability
  profiles).
- CLI subprocess: **`claude_cli`** (env-var stripping for case-
  insensitive matches, kill-on-drop) + **`gemini_cli`**.
- In-process: **`MockProvider`** (canned text / event-sequence /
  error variants) for tests.
- Shared infra: `HttpProviderBase` (single reqwest client, SSE idle
  timeout via `tokio::time::timeout` per chunk), `ToolCallBuffer`
  (parallel-call accumulator with three-stage args parsing),
  `ResolvedAuth` enum with manual redacting `Debug`,
  `ProviderRegistry` with TOML-driven `from_config` factory.

### Audit fixes — A.R.C. self-review tier
Three rounds of automated audit (run IDs `3ab2b7fa`, `65be2621`),
~250 findings total, ~50 fixed across these commits:
- **`9683ce8`**: round-1 critical + 9 errors (UTF-8 boundary panic in
  HTTP error truncation, ContentType=text/event-stream content
  classification, etc.).
- **`67de40d`**: 22 more errors. Highlights: `Capabilities::validate()`,
  `Pricing::cost_for` debug_assert + saturation log, portable
  `systemtime_millis` serde, `Capabilities` modality validation,
  `ChatResponse::into_events()` for cache replay,
  `CacheHitInfo.replayed_from_cache`, OpenAI 7+22 `pending_stop_reason`
  for the usage-only-chunk bug, mock-5 single-Mutex,
  `BasicAuthResolver` warn-on-Inline.
- **`cf1605e`**: round-2 (15 issues). Highlights: `SecretString::Serialize`
  redacts (`<redacted>` instead of plaintext), `ConfigError::Parse`
  preserves `#[source]` chain, `Auth::env` distinguishes
  `VarError::NotPresent` vs `NotUnicode`, `MockProvider::stream`
  no-panic on poisoned mutex, real-bounded HTTP error body read
  (`read_bounded_body` streams chunks, caps at 8 KiB — round-1 only
  swapped the marker; the body was still fully buffered),
  `http_extras` logs invalid header names, every string-id `new()` and
  `From<&str>` panic on empty, `ToolCall::new` runtime `assert!`
  (was `debug_assert!`), `usage::cost_for` adds release-build warn
  on saturation, **deleted `transport.rs`** (TODO O-1 trigger fired:
  hit pipeline MVP without anyone needing the `HttpTransport` trait).

---

## Pre-roadmap moves

These don't fit a milestone label cleanly but were load-bearing
decisions worth recording.

### Renamed CLI provider modules (early)
- `claude_subprocess` → `claude_cli`, added `gemini_cli`. Reflects
  the subscription-auth model rather than the implementation detail
  (`subprocess`).

### Added MLX + llama.cpp HTTP-server backends (`a81378e`)
- Both expose OpenAI-compatible HTTP, so they're thin builders over
  `OpenAiProviderBuilder` with their own capability profiles + default
  ports (both 8080 — pick one per host or override `--port` on
  whichever launches second).
- Distinct `ProviderConfig` variants instead of leaning on
  `OpenaiCompat` so logs / config read deployment posture at a glance
  (`type = "mlx"` vs `type = "llamacpp"` vs `type = "vllm"`).
- vLLM stays the cloud-deployment story (PagedAttention + batching
  shines on A100/H100). Inline docs flag vLLM as overkill for local.
