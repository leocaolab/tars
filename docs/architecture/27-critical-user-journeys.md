# 27 — Critical User Journeys (CUJ-1 .. CUJ-14)

This document is the **canonical, numbered definition** of the Critical User
Journeys that the Features table (and downstream requirement/test work)
traces to. Each CUJ has a stable `CUJ-N` anchor so other documents can cite
it as `docs/architecture/27-critical-user-journeys.md:CUJ-N` (or by line).

Mapping discipline (read before using the table):

- **Every shipped feature traces to ≥1 CUJ; every CUJ is served by exactly
  one *primary* feature.** The feature→CUJ relation is therefore
  many-features-to-one-CUJ is *disallowed* for the primary tag, but one
  feature MAY reference sibling CUJs in its boundary notes (e.g. F14's note
  points at F9/F11). Those boundary cross-references are *not* additional
  primary mappings — they are pointers that say "this concern is served
  elsewhere," and they never re-map a CUJ's primary feature.
- **Status is split two ways.** A CUJ is either **Satisfied today**
  (a shipped feature completes the journey) or **Tracked / not satisfied**
  (the CUJ exists to make a gap explicit; no shipped feature completes it).
  The four DEFERRED CUJs (11, 12, 14) and the partially-deferred ones are
  marked so a reviewer never reads a tracked-but-unsatisfied CUJ as closed.

| CUJ | Actor | Primary feature | Satisfied today? |
|-----|-------|-----------------|------------------|
| CUJ-1 | Python SDK caller | F1 | Yes (from-source install) |
| CUJ-2 | App developer | F2 | Yes |
| CUJ-3 | Agent developer | F3 | Yes (built-in tools, in-mem history) |
| CUJ-4 | Platform/infra developer | F4 | Yes |
| CUJ-5 | App developer (safety/format) | F5 | Yes (caller-authored output scrub) |
| CUJ-6 | App developer / SRE | F6 | Yes |
| CUJ-7 | Resilience-seeking developer | F7 | **Rust crate API only — no Python binding** |
| CUJ-8 | Operator / SRE | F8 | Yes |
| CUJ-9 | Latency-sensitive app developer | F10 | Yes on HTTP/SSE; Python binding non-incremental |
| CUJ-10 | Spend-controlling developer | F9 | Cost-**ordering** yes; hard cap Rust-only; cache discount ignored |
| CUJ-11 | MCP-tool developer | F13 | **No — tracked target (no `tars-tools` impl)** |
| CUJ-12 | Production operator (durability) | F12 | **No — tracked target (in-mem history)** |
| CUJ-13 | Operator (observability) | F11 | Yes (opt-in OTLP) |
| CUJ-14 | Multi-tenant operator | F14 | **No — tracked target (M6 not started)** |

---

## CUJ-1 — Add an LLM completion without hand-rolling provider HTTP/retries/accounting

**Actor:** Application developer (Python SDK caller).
**Trigger:** Wants an LLM completion in their app without writing provider
HTTP, retries, or token accounting.

**Steps**
1. **Install (source-only today** — there is no PyPI wheel; `docs/comparison.md:216`
   confirms "No PyPI / npm release yet"): install Rust 1.85+, clone tars, and
   in `crates/tars-py` run `maturin develop --release`. This is a from-source
   build, not `pip install`, and is the only supported install path today.
2. `cargo run -p tars-cli -- init` writes `~/.tars/config.toml` with starter providers.
3. `export ANTHROPIC_API_KEY=sk-ant-...`.
4. `p = tars.Pipeline.from_default("anthropic")`.
5. `p.complete(model="claude-sonnet-4-5", system=..., user=..., max_output_tokens=2000, thinking=True)`.
6. Read `resp.text`, inspect `resp.usage` (input/output/cached/thinking) and
   `resp.telemetry` (cache_hit, retry_count, layer trace, latency).

**Success:** After a from-source maturin build, a reply returns through a
default Pipeline that auto-engages telemetry, cache, and retry; per-call usage
and telemetry are observable on the response object without the developer
writing any provider, retry, or accounting code. The from-source build is
stated up front as the adoption cost.

---

## CUJ-2 — Switch model/provider without rewriting call sites

**Actor:** Application developer.
**Trigger:** Needs to swap backend (cost, availability, or an existing
subscription) without rewriting call sites.

**Steps**
1. Open `~/.tars/config.toml` (or use a built-in provider id).
2. Change the provider argument from `"anthropic"` to e.g. `"openai"`,
   `"deepseek"`, or a subscription path `"claude_cli"` / `"gemini_cli"` / `"codex_cli"`.
3. For DeepSeek, `export DEEPSEEK_API_KEY=...`; for CLI providers, no API key —
   tars reuses the vendor CLI's existing subscription session.
4. Re-run the same `complete()` / `call()` call site unchanged.
5. Optionally verify: `cargo run -p tars-cli -- run -p deepseek "Say hi in 5 words."`.

**Success:** The same call site runs identically against a different backend
behind the one `Provider` trait; tool-use, streaming, thinking, and retry
semantics are normalized so the swap does not break at provider edge cases.
Cost-driven *automatic* routing is a separate journey — see **CUJ-10**.

---

## CUJ-3 — Tool-using agent that loops to a final answer

**Actor:** Application developer building an agent.
**Trigger:** Wants a model to call tools and loop autonomously to a final
answer instead of hand-orchestrating tool dispatch.

**Steps**
1. Register tools in a `ToolRegistry` using the in-tree `tars-tools` built-ins
   (bash/grep/glob/fs). MCP-backed registration is NOT implemented in
   `tars-tools` (MCP appears only in architecture docs — see **CUJ-11**).
2. Create a `Session` with the registry attached.
3. Call `Session::send` with the user task.
4. tars auto-dispatches each requested tool (parallel calls dispatched in order
   and packaged into one user message with N tool_result blocks), re-invokes
   the model, and repeats until a text-only reply.
5. Each turn is built through a `TurnGuard` that rolls back on Drop (early `?`,
   panic, or cancellation) and commits only on success.
6. Read the final `Response`; `session.history_version` incremented once per
   visible mutation. NOTE: `Session` history is an in-memory `Vec<Message>`
   (`crates/tars-runtime/src/session.rs:149`) — NOT persisted; a mid-conversation
   restart loses history (durability gap → **CUJ-12**).

**Success:** A multi-step tool-using agent runs to a final text answer with
correct multi-tool result packaging and atomic per-turn history (no
half-committed turns on error/cancel), without the developer writing the tool
loop — using the in-tree built-in tools that actually ship.

---

## CUJ-4 — Fail fast at startup on provider/role capability mismatch

**Actor:** Platform/infra developer assembling a multi-role agent system.
**Trigger:** Wants to fail fast at startup if a role's provider cannot satisfy
the role's request shape, rather than crashing on the first live request.

**Steps**
1. Define role requirements with `tars.CapabilityRequirements` (e.g. planner
   `requires_thinking=True`; executor `requires_tools=True`,
   `estimated_max_output_tokens=8000`; reviewer `requires_structured_output=True`).
2. For each role, build `Pipeline.from_default(provider_for(role))`.
3. Call `p.check_capabilities(reqs)` (no model call).
4. If falsy, read `r.reasons` (ToolUseUnsupported, ThinkingUnsupported,
   ContextWindowExceeded, MaxOutputTokensExceeded) and exit non-zero.
5. At request time the routing layer also runs `compatibility_check` against
   each candidate before dispatch, aggregating all incompatibilities.

**Success:** Provider/role mismatches are caught at startup (and again
pre-dispatch) as typed, fully-aggregated reasons without a network round-trip;
when no candidate fits, the caller gets `NoCompatibleCandidate` with per-provider
skipped reasons rather than a string error.

---

## CUJ-5 — Guarantee output is valid/scrubbed before it reaches caller code

**Actor:** Application developer with safety/format requirements.
**Trigger:** Must guarantee output is valid (e.g. parseable JSON) or scrubbed
of PII before it reaches caller code.

**Steps**
1. Write Python validator callbacks returning `tars.Pass()`,
   `tars.Reject(reason=...)`, or `tars.FilterText(text=...)`.
2. Pass them ordered: `Pipeline.from_default("anthropic", validators=[("strip_pii", strip_pii), ("must_be_json", must_be_json)])`.
3. Call `complete()`; validators chain in order, each seeing the prior's filtered output.
4. On `Reject`, catch `TarsProviderError(kind="validation_failed")` (classified
   Permanent, so RetryMiddleware does not resample) and resample with explicit
   prompt variation at the caller's own layer.
5. A buggy validator (raises/wrong type) is translated into the same permanent
   error rather than crashing the worker.

**Success:** Every response is validated/filtered post-model and pre-caller in
declared order; rejects are typed and never auto-retried, and user-side
validator bugs cannot crash the runtime. PII scrubbing here is caller-authored
validator logic on the *output*; "mandatory PII redaction" of telemetry/MELT is
a separate, target concern — see **CUJ-13** / **CUJ-14**.

---

## CUJ-6 — React programmatically to provider failures via typed errors

**Actor:** Application developer / SRE handling production failure modes.
**Trigger:** Needs to react programmatically to provider failures (rate limits,
exhausted routing, unknown tool) instead of parsing error strings.

**Steps**
1. Wrap the call in try/except.
2. Catch `tars.TarsRoutingExhaustedError` and iterate `e.skipped_candidates`
   (list of `(provider_id, [CompatibilityReason])`) to log why each candidate
   was filtered.
3. Catch `tars.TarsProviderError`; branch on `e.kind`: `rate_limited` → sleep
   `e.retry_after or 30`; `unknown_tool` → register `e.tool_name`; else if
   `e.is_retriable` treat as final failure after the pipeline already retried.
4. Rely on the `TarsError` → TarsConfigError/TarsProviderError/TarsRuntimeError
   hierarchy so a generic `except TarsProviderError` still matches subclasses.

**Success:** Failure handling is driven by typed errors and structured fields
(retry_after, tool_name, skipped_candidates) with a stable class hierarchy, so
callers branch reliably without string-matching.

---

## CUJ-7 — Hedge a task across N agents at task granularity

**Actor:** Developer wanting task-level resilience.
**Trigger:** Wants to reduce tail latency / tolerate a slow-or-failing agent by
racing the same task across multiple agents.

> **Reachability boundary (load-bearing):** This journey targets the
> **agent-level** hedge `EnsembleAgent::new(id, role, candidates)` +
> `ensemble.run(task, ctx)`, which exists ONLY in Rust
> (`crates/tars-runtime/src/ensemble_agent.rs:24,35,71`). It has **no Python
> binding** — `grep -rn 'EnsembleAgent|TarsAgent' crates/tars-py` (minus
> `.venv`) returns nothing. There is no built-in **completion-level** ensemble
> type any more — provider selection is not a pipeline concern, so a
> completion-level hedge is just a caller composition (build N `LlmService`s,
> call all, merge) rather than an `EnsembleService`. This CUJ is the
> agent-level hedge *above* that, satisfied at the **Rust crate API surface
> only**, not at the Python caller surface.

**Steps**
1. Implement/obtain N agents satisfying
   `trait Agent { id, role, skills, async run(&self, task: Task, ctx: AgentContext) }`
   (e.g. several `TarsAgent`s over different providers, and/or a native non-LLM agent).
2. Wrap them in `EnsembleAgent::new(id, role, candidates)`.
3. Build an `AgentContext` (it carries the cancellation token + telemetry
   plumbing the ensemble uses to cancel losers) and a `Task` (user intent, not
   a ChatRequest).
4. Call `ensemble.run(task, ctx).await` (signature per
   `crates/tars-runtime/src/ensemble_agent.rs:71` — `ctx` is REQUIRED, not optional).
5. The ensemble gives each candidate a child cancel token derived from
   `ctx.cancel`, runs the task on all N concurrently, returns the first success,
   and cancels the rest via their child tokens.
6. Internally each TarsAgent turns the task into a prompt and drives its own
   white-box tool loop over a pure-inference provider.

**Success:** One task is hedged across N agents at task granularity (above the
pipeline's completion-level ensemble) and returns the first successful result;
`AgentContext.cancel` is what lets the winner cancel the slow/failing losers.
**This is a Rust-crate-API capability; a Python caller cannot reach it today.**

---

## CUJ-8 — Validate a provider/config and benchmark backends from the shell

**Actor:** Operator / SRE.
**Trigger:** Wants to confirm a provider is reachable and a config is valid
before shipping, and to benchmark backends.

**Steps**
1. `cargo run -p tars-cli -- init` to scaffold config.
2. Set the referenced credential env var.
3. `cargo run -p tars-cli -- run -p <provider> "Say hi in 5 words."` and confirm
   the reply on stdout plus the one-line `usage:` summary on stderr.
4. Use the CLI `probe` (pre-flight capabilities) and `bench` (N iterations;
   reports TTFB / total / decode tok/s) subcommands.
5. If `error in tars run:` appears, reconcile the env var name with what
   `~/.tars/config.toml` declares for that provider.

**Success:** A provider/config is validated end-to-end from the shell with a
clear pass (reply + usage) or actionable failure, and backends can be
benchmarked before being wired into application code.

---

## CUJ-9 — Stream model output token-by-token to the end user

**Actor:** Latency-sensitive front-end / app developer.
**Trigger:** Wants to stream model output token-by-token as generated, rather
than blocking until the full reply is ready — the UX that justifies streaming
being "mandatory" (`docs/architecture/12-api-specification.md:15`).

**Steps**
1. Run tars in service shape: `cargo run -p tars-server` (binds loopback by
   default; warns loudly on a non-loopback bind).
2. POST to `/v1/complete/stream` with the completion request; the server
   returns Server-Sent Events.
3. Consume the SSE stream incrementally (e.g. EventSource / streaming HTTP
   client), rendering each delta as it arrives, so first-token latency drives the UX.
4. Normalized streaming semantics are produced by the same Pipeline used
   synchronously, so retry/telemetry behavior is consistent.

> **Reality note (load-bearing):** The Python SDK does NOT currently expose
> incremental tokens — `run_complete_tagged` drains the whole LLM stream into a
> single `Response` before returning (`crates/tars-py/src/lib.rs:1828` "Drain the
> LLM stream into a Response"). Incremental consumption is available today only
> via the server's SSE endpoint, not via `Pipeline.complete()` in Python.

**Success:** A client receives and renders tokens incrementally over SSE from
`POST /v1/complete/stream`. The journey explicitly scopes incremental streaming
to the HTTP/SSE surface and flags that the Python in-process binding only
returns a fully-drained Response.

---

## CUJ-10 — Automatically prefer the cheapest compatible provider

**Actor:** Application developer / platform owner controlling provider spend.
**Trigger:** Cost is the #1 reason to switch backends (CUJ-2); wants requests to
automatically prefer the cheapest provider that can serve them.

**Steps**
1. Have multiple providers configured (e.g. a free local model plus a paid API model).
2. **Provider selection is a caller composition, not a pipeline layer** — there
   is no built-in router. For each candidate provider, skip it when
   `req.compatibility_check(provider.capabilities())` returns `Incompatible`,
   and score the survivors by *estimated* cost of THIS request from each
   provider's static pricing
   (`provider.capabilities().pricing.estimate_chat_cost(&req, default_max_output)`,
   `crates/tars-types/src/usage.rs:160-193`).
3. Pick the cheapest compatible candidate and build a service over it —
   `LlmService::of(provider, model)`, or `LlmService::default_chain(provider,
   model, opts)` for the full onion — then `call()` it.
4. Keep the remaining candidates as an ordered fallback list: on error, try the
   next (fallback is also a caller composition, not a middleware).
5. If you cache, key per provider (a shared cache could serve one provider's
   response for another): give each candidate's service its own `cache_origin`.

> **Cost-accuracy boundary (load-bearing — the `cost-accounts-for-cache`
> invariant).** `Pricing::estimate_chat_cost`
> (`crates/tars-types/src/usage.rs:160-193`) charges **all** input chars at the
> full `input_per_million` rate (usage.rs:173,179) and **never** references
> `cached_input_per_million` — even though the `Pricing` struct carries that
> field (usage.rs:116, "typical 25-50% of standard") and `is_zero()` checks it
> (usage.rs:143). Because a cost-preferring caller sorts candidates by exactly
> this cache-blind estimate, the cost ORDERING between a cache-heavy provider
> (e.g. Anthropic re-sending a ~90% prompt-cached system prompt) and a no-cache
> provider can **invert**: the pre-call sort prices every input token at full
> rate regardless of prompt-cache discounts. The post-call `cost_for`
> (usage.rs:195+) *does* honor cached/cache-creation rates, so reconciliation is
> correct; it is only the **selection sort key** that is cache-blind. Treat
> cost-ordered selection as "cheapest-first by full-rate estimate," NOT
> "cheapest-first accounting for prompt cache."

**Scope limits (grounded in code/TODO):**
- (a) There is **no built-in cost router** any more: provider selection was
  removed from the pipeline (it's a caller composition), so the cheapest-first
  loop above is a Rust-surface pattern over `compatibility_check` +
  `estimate_chat_cost` — not a one-argument policy, and not reachable from the
  Python `Pipeline` surface (which exposes `check_compatibility` but not the
  pricing estimate).
- (b) The hard "cap my spend / reject over budget" path —
  `PerCallBudgetMiddleware` (`tars-pipeline/src/middleware/budget.rs`) — is
  shipped in Rust as an opt-in `.layer(...)` but is NOT exposed in the Python
  surface today.
- (c) Per-tenant *running-total* budget enforcement (`TenantBudgetMiddleware`,
  `tars-pipeline/src/middleware/tenant_budget.rs`) and the Auth/IAM/Budget onion
  are M6 work, "still not started" per `TODO.md` (see **CUJ-14**).

**Success:** A Rust developer prefers the cheapest-compatible provider by
composing `compatibility_check` + `estimate_chat_cost` over the candidate
providers, then builds one `LlmService` over the winner (with the rest as an
ordered fallback). The journey is honest about three boundaries: the ordering
is by a **cache-blind** full-rate estimate; provider selection is a caller
composition with no Python binding; and per-tenant cumulative budget
enforcement is unstarted M6.

---

## CUJ-11 — Register MCP-backed tools  *(TARGET / FUTURE — not yet implementable)*

**Actor:** Application developer wanting MCP-backed tools.
**Trigger:** Wants to register tools served by an external MCP (Model Context
Protocol) server, since MCP is named in the architecture docs
(`docs/architecture/05-tools-mcp-skills.md`).

**Status:** **TRACKED, NOT SATISFIED.** No implementation exists in
`tars-tools` today — MCP appears only in `docs/architecture/*`, with **zero
`mcp` references in `crates/tars-tools/src`** (verified). This CUJ has no
shipped satisfying feature; F13 is its tracked target.

**Target-state steps (when built):**
1. Configure and launch an MCP server.
2. Register its advertised tools into a `ToolRegistry` via an MCP adapter.
3. Run a `Session` so the agent dispatches MCP-backed tool calls through the
   same loop as built-in tools (CUJ-3).
4. Handle an MCP serve-failure as a typed `unknown_tool` / `ToolError` (CUJ-6).

Until then, the only tools that register and dispatch are the in-tree
`tars-tools` built-ins (bash/grep/glob/fs).

**Success:** DEFERRED — this CUJ documents the intended journey and is
explicitly labeled target/future. Conformance is N/A until an MCP adapter lands;
today's success criterion is only that the gap is visibly tracked.

---

## CUJ-12 — Survive a mid-task restart  *(TARGET / FUTURE — durability gap)*

**Actor:** Operator / SRE running tars in production.
**Trigger:** The product is positioned to "serve agents in production with the
same predictability as a database" (README). A skeptic's first question: the
process restarts mid-task — is the conversation/turn lost?

**Status:** **TRACKED, NOT SATISFIED.** Current reality: `Session` history is an
in-memory `Vec<Message>` (`crates/tars-runtime/src/session.rs:149`) with NO
persist/resume/recover path — a restart loses the in-flight conversation. The
adjacent `EventStore` / trajectory layer (`tars-runtime` event.rs + `tars-storage`,
surfaced in Python as `event_store_dir`) captures step events / LLM-call
captures for **replay of trajectories**, NOT for resuming a live `Session`'s
chat history. F12 is this CUJ's tracked target.

**Target-state steps (when built):**
1. Configure a durable session store.
2. On each committed turn (the `TurnGuard` commit point), persist the new
   history snapshot keyed by `(session_id, history_version)`.
3. After a crash/restart, reconstruct the `Session` from the last persisted
   `(session_id, history_version)` and continue.
4. Correlate logs/telemetry to the same `(session_id, history_version)` pair.

**Success:** DEFERRED / labeled target — names session durability +
crash-recovery as a MISSING journey, grounds the gap in the in-memory
`Vec<Message>` and replay-only `EventStore`, and sketches the persist/resume
design. Not claimed shippable today.

---

## CUJ-13 — Turn on observability export and read correlated traces

**Actor:** Operator / SRE turning on observability.
**Trigger:** Observability is a headline differentiator vs LangChain; wants to
enable telemetry export and read request-correlated traces, before any
multi-tenant work.

**Steps**
1. Build/run a tars binary with the opt-in `otlp` feature
   (the OTLP exporter lives behind `--features otlp` in `tars-melt`).
2. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to an OTLP collector / Grafana Tempo /
   Jaeger backend (export activates only when the feature is on AND the endpoint
   is set; otherwise the same stderr fmt logs are emitted).
3. Optionally tune head sampling via `OTEL_TRACES_SAMPLER_ARG` (parent-based
   traceidratio; default AlwaysOn).
4. Telemetry is initialized once per binary through `tars_melt::init` (used by
   `tars-cli`), with a `CardinalityGuard` capping high-cardinality label keys.
5. Drive a completion (CLI `run`, or the server) and read the trace/metrics in
   the backend; per-call telemetry (cache_hit, retry_count, layer trace, latency)
   is also readable on the `Response`.

> **Scope note:** "mandatory PII redaction" of telemetry is an ASSERTED property
> tied to the unstarted M6 multi-tenant stack (**CUJ-14**), NOT a verified
> guarantee of today's MELT export — do not claim redaction as shipped here.

**Success:** An operator turns on the opt-in OTLP exporter, points it at a real
collector, and sees tars-internal traces/metrics (cardinality-guarded,
head-sampled) — a standalone, runnable observability journey that does not
depend on the multi-tenant CUJ. The PII-redaction claim is explicitly scoped out.

---

## CUJ-14 — Serve multiple tenants without cross-tenant leakage  *(TARGET / FUTURE — NOT SHIPPED)*

**Actor:** Operator / SRE running tars as a multi-tenant service.
**Trigger:** Wants one agent runtime to serve multiple tenants without one
tenant's prompts poisoning another's cache or leaking data.

**Status:** **TRACKED, NOT SATISFIED — the single most important
missing-reality item.** `crates/tars-server/src/lib.rs:5-6` states verbatim "the
M6 server subset with the multi-tenant security stack deliberately left out: no
auth, no IAM, no per-tenant isolation" and the server binds loopback only.
`TODO.md` confirms the real M6 (`tars-security` crate, per-tenant isolation +
lifecycle, Auth/IAM/Budget middleware onion, gRPC, Postgres/Redis stores) is
"still not started." F14 is this CUJ's tracked target.

**Target-state steps (when M6 lands):**
1. Deploy tars in service shape with L2 (Redis/SQL) + L3 (S3) cache backing the
   same Cache Registry trait used locally with L1 in-mem.
2. Configure providers/secrets via the 5-layer config override with `Secret`
   refs (env-var references, no secrets in files).
3. Issue requests carrying tenant identity; the pipeline front-loads Auth/IAM
   and enforces tenant isolation in cache via content-addressed keys + ref
   counting + tenant-scoped triple defense.
4. Enforce per-tenant running-total budgets via `TenantBudgetMiddleware` once
   wired (blocked on a persistent BudgetStore/KVStore per TODO).
5. Observe per-tenant MELT with PII redaction (the redaction property is part of
   this future stack — see CUJ-13's scope note).
6. Correlate logs via `(session_id, history_version)`.

**Success:** DEFERRED / clearly labeled future — this is the single most
important missing-reality item, marked explicitly aspirational/M6 roadmap. Its
asserted properties (L2/L3 cache backing, Auth/IAM, per-tenant cache isolation,
content-addressed keys + ref-counting + triple defense, mandatory PII redaction,
Secret refs) are documented as targets so no conformance check treats them as
shipped. **Standalone capabilities that DO ship are split out into their own
CUJs with their own primary features — cost routing (CUJ-10 → F9), observability
(CUJ-13 → F11) — so referencing them here is a pointer, not a re-mapping of
CUJ-14's primary feature (which remains F14, unsatisfied).**
