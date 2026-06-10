# Doc 00 — Overview and Navigation

> This document set defines the complete architecture of **TARS Runtime** — a Rust-implemented, general-purpose LLM Runtime for multi-Agent collaboration.
>
> Status: design precedes implementation. All documents represent the **target architecture**; implementation aligns to it incrementally by milestone.

---

## 1. What This Is

TARS is a **general-purpose Agent Runtime**, with the following core positioning:

- **Infrastructure, not application** — provides a unified substrate for Agent orchestration / LLM invocation / Tool integration; business Agents are built on top
- **Rust-first** — the core engine is written in Rust, targeting high concurrency, low latency, and memory safety; exposed to other languages via FFI / HTTP
- **Multi-Provider abstraction** — simultaneously supports OpenAI / Anthropic / Gemini API + CLI + local inference (vLLM / mistral.rs / ONNX)
- **Multi-Agent collaboration** — layered Orchestrator + parallel Workers + Critic Loop + Trajectory tree + event sourcing
- **Multiple deployment shapes** — Personal Local-First (BYOK) / Team Self-Hosted / SaaS Multi-tenant / Hybrid Cloud Plane
- **Production-grade** — multi-tenant hard isolation / security guards / observability / backup and restore / incident response, all included

**What it is not**:
- Not a new LLM model
- Not a ChatGPT-style product (it is the substrate for building such products)
- Not a LangChain replacement (positioned lower-level, more industrial)
- Not a single-Agent framework (designed multi-Agent by default)

---

## 2. Design Philosophy

The entire document set is governed by the following 8 core principles:

### 2.1 Layered, Not Monolithic
Each layer has a single responsibility; adjacent layers are decoupled via traits. Any layer's implementation can be swapped independently (e.g. switching Provider vendors, or switching Storage from Postgres to SQLite).

### 2.2 Plan as DAG, Execute as State Machine
Agent orchestration is expressed as a DAG at the planning stage; at runtime it is an event-sourced state machine with backtracking, looping, and abandonment. The two are not conflated.

### 2.3 Tenant Isolation is Sacred
Cross-tenant leakage of data / computation / side effects is treated as the most severe class of defect. No performance optimization (e.g. shared cache) may breach the tenant boundary.

### 2.4 Fail Closed
All security mechanisms reject the request on failure — never "default allow". This applies to Auth / IAM / Cache / Budget / Schema / Side Effect alike.

### 2.5 Observable by Construction
Observability (M/E/L/T) is not bolted on after the fact; it is guaranteed architecturally — every component, every call, every state transition produces a queryable signal.

### 2.6 Trust Nothing You Didn't Compute
LLM outputs, user inputs, Tool returns, MCP server behavior — everything external is untrusted, and may only affect system state through explicit filters.

### 2.7 Cost is a First-Class Concern
LLM calls account for 95%+ of cost. All architectural decisions (cache / routing / model tier / budget) revolve around cost controllability.

### 2.8 Single Source of Truth
The Rust trait is the source of truth; HTTP API / gRPC / Python / TypeScript and so on are projections of it. No binding is permitted to deviate from the core semantics.

---

## 3. Overall Architecture

```
                       ┌──────────────────────────────┐
                       │   Frontend Adapters (Doc 07) │
                       │   CLI / TUI / Web / CI       │
                       └──────────────┬───────────────┘
                                      │
                       ┌──────────────▼───────────────┐
                       │   API Layer (Doc 12)         │
                       │   Rust / HTTP+SSE / gRPC     │
                       │   Python(PyO3) / TS(napi-rs) │
                       └──────────────┬───────────────┘
                                      │
                       ┌──────────────▼───────────────┐
                       │   Agent Runtime (Doc 04)     │
                       │   Trajectory Tree + Events   │
                       │   + Backtrack + Recovery     │
                       └────┬─────────────────┬──────┘
                            │                 │
              ┌─────────────▼─────┐  ┌────────▼─────────┐
              │  Tools / Skills   │  │  PromptBuilder   │
              │  (Doc 05)         │  │  (Doc 04 §11)    │
              │  Tool/MCP/Skill   │  │  Static Prefix / │
              │  3-layer abstract │  │  Project Anchor /│
              └─────────┬─────────┘  │  Dynamic Suffix  │
                        │            └────────┬─────────┘
                        │                     │
                        └──────────┬──────────┘
                                   │
                       ┌───────────▼───────────────┐
                       │  Middleware Pipeline      │
                       │  (Doc 02)                 │
                       │  Telemetry → Auth → IAM   │
                       │  → Budget → Cache → Guard │
                       │  → Routing → Breaker      │
                       └───────────┬───────────────┘
                                   │
              ┌────────────────────┼─────────────────┐
              │                    │                 │
   ┌──────────▼──────┐  ┌──────────▼─────┐  ┌──────▼──────┐
   │ Cache Registry  │  │ LLM Provider   │  │ Tool/MCP    │
   │ (Doc 03)        │  │ (Doc 01)       │  │ Subprocess  │
   │ L1/L2/L3 + ref  │  │ HTTP / CLI /   │  │ (Doc 05)    │
   │ counting +      │  │ Embedded       │  │ long-lived  │
   │ Janitor         │  │ adapters       │  │ + isolation │
   └─────────────────┘  └────────────────┘  └─────────────┘

   ▲ Cross-cutting layers (depended on by all upper layers)
   │
   ┌─────────────────────────────────────────────────────────┐
   │ Storage (Doc 09): Postgres / SQLite / Redis / S3        │
   │ Config + Multi-tenancy (Doc 06): 5-layer override       │
   │ Security Model (Doc 10): Auth / IAM / Encrypt / Audit   │
   │ MELT Observability (Doc 08): Metrics / Events / Logs / Traces │
   │ Performance (Doc 11): SLO / Capacity / Bench           │
   │ Operations (Doc 13): Runbook / Incident / Backup       │
   └─────────────────────────────────────────────────────────┘
```

---

## 4. Document Index

| Doc | Title | Core Content | Best Read First By |
|---|---|---|---|
| **00** | Overview and Navigation (this doc) | Project intro / doc relationships / reading paths | Everyone |
| [01](./01-llm-provider.md) | LLM Provider Abstraction | Unified trait for 9 backend classes; CLI subprocess reuse; three-stage tool call; cache directives | LLM integration developers |
| [02](./02-middleware-pipeline.md) | Middleware Pipeline | 10-layer onion model; IAM up-front; dual-channel Guard; Cancel propagation | Business logic developers |
| [03](./03-cache-registry.md) | Cache Registry | Three-tier cache (L1/L2/L3); content addressing; reference counting; three lines of defense for tenant isolation | Performance / cost engineers |
| [04](./04-agent-runtime.md) | Agent Runtime | Trajectory tree; event sourcing; Saga compensation; recovery mechanism; Frontend contract | Core architects |
| [05](./05-tools-mcp-skills.md) | Tools / MCP / Skills | Clear separation of three-layer concepts; MCP integration; three Skill implementations | Tool developers |
| [06](./06-config-multitenancy.md) | Config and Multi-tenancy | 5-layer override; lock layer; Secret management; tenant lifecycle | DevOps / platform engineering |
| [07](./07-deployment-frontend.md) | Deployment and Frontend | 4 deployment shapes; CI / TUI / Web Dashboard; Hybrid control plane | Product + DevOps |
| [08](./08-melt-observability.md) | MELT Observability | Disambiguation of three data flows; cardinality control; mandatory redaction of sensitive data | SRE |
| [09](./09-storage-schema.md) | Storage Schema | Postgres + SQLite + Redis + S3; partitioning; migration; tenant-level cleanup | Database engineers |
| [10](./10-security-model.md) | Security Model | STRIDE threat model; trust boundaries; isolation summary; Prompt Injection defenses | Security engineers |
| [11](./11-performance-capacity.md) | Performance and Capacity | SLO definitions; bottleneck analysis; cache ROI; load-testing methodology | Performance engineers + SRE |
| [12](./12-api-specification.md) | API Specification | Rust / HTTP / gRPC / Python(PyO3) / TS(napi-rs) / WASM | SDK developers |
| [13](./13-operational-runbook.md) | Operational Runbook | On-call playbook; 12 failure scenarios; backup and restore; incident communication | SRE / On-call |
| [14](./14-implementation-path.md) | Implementation Path | M0–M9 milestones; crate workspace; dependency choices | Maintainers / planners |
| [15](./15-output-validation.md) | Output Validation | Validator chain; Pass/Reject/Filter/Annotate outcomes | Pipeline developers |
| [16](./16-evaluation-framework.md) | Evaluation Framework | Event-channel + metrics-sink plumbing. **§7.1 per-call deterministic scoring superseded by Doc 18** | Eval developers |
| [17](./17-pipeline-event-store.md) | Pipeline Event Store | Per-call observability DB; CAS bodies; `tars events` | Observability / eval |
| [18](./18-agent-testing.md) | **Agent & LLM Testing** | Behavioral diff (not text); 5 test modes (invariant / metamorphic / mutation / golden / quality); paired stats (McNemar); native LLM judge via `claude_cli`; framework-vs-domain discipline | Anyone testing/migrating prompts, data, or models |
| [20](./20-agent-abstraction.md) | **Agent Abstraction** | The `Agent` contract from the user's view: hand a `Task` to a `SkillSet`; native vs user agents | SDK authors / agent builders |
| [21](./21-tars-agent-impl-notes.md) | **TarsAgent Impl Notes** | Native-agent build notes; the two-`ToolRegistry` unification; open decisions | Maintainers |

---

## 5. Reading Paths

Different roles will be most efficient reading in the following order:

### 5.1 I am a core architect
```
00 (this doc) → 04 (core Runtime) → 02 (Middleware) → 01 (Provider)
→ 03 (Cache) → 05 (Tools) → 10 (Security) → 06 (Config)
→ rest as needed
```

### 5.2 I want to develop a new Provider adapter for TARS
```
00 → 01 (Provider trait deep dive) → 02 (understand Provider's place in the Pipeline)
→ 12 §4-5 (HTTP/gRPC protocol reference) → done
```

### 5.3 I want to build a new Frontend (Web / mobile / IDE plugin)
```
00 → 04 §12 (TrajectoryEvent contract) → 12 (API selection)
→ 07 (Frontend Adapter pattern) → done
```

### 5.4 I want to integrate from Python / TypeScript
```
00 → 12 §6 (Python) or §7 (TypeScript) → 04 §12 (understand the event stream)
→ 12 §10 (Conformance tests) → done
```

### 5.5 I am SRE / DevOps
```
00 → 13 (Runbook) → 06 (multi-tenant config) → 09 (storage)
→ 11 (performance / capacity) → 08 (observability) → 07 (deployment shapes) → 10 (security)
```

### 5.6 I am a security engineer
```
00 → 10 (security model) → 06 §4 (tenant isolation) → 03 §10 (cache isolation)
→ 02 §4.5 (Prompt Guard) → 13 §5.10 (Isolation Breach response) → 08 §11 (redaction)
```

### 5.7 I am product / decision-maker
```
00 → 07 (comparison of 4 deployment shapes) → 11 §8 (cost structure)
→ 13 §15 (post-mortem culture) → done
```

### 5.8 I just joined the team and want a comprehensive overview within 1 week
```
Day 1: 00 + 04 (core architecture)
Day 2: 02 + 01 (request path)
Day 3: 03 + 05 (Cache + Tools)
Day 4: 06 + 10 (config + security)
Day 5: 07 + 12 (deployment + API)
Day 6: 08 + 09 + 11 (operations trio)
Day 7: 13 (Runbook) + Q&A
```

---

## 6. Document Dependencies

Other documents that each doc depends on (dashed lines are weak dependencies):

```
                          ┌────────┐
                          │   00   │
                          └────┬───┘
                               │
     ┌──────────────┬──────────┼──────────┬───────────────┐
     │              │          │          │               │
  ┌──▼──┐        ┌──▼──┐    ┌──▼──┐    ┌──▼──┐         ┌──▼──┐
  │ 01  │◄───────┤ 02  ├────┤ 04  ├────┤ 05  │         │ 12  │
  │ Pro │        │ Mid │    │ Run │    │Tool │         │ API │
  └──┬──┘        └──┬──┘    └──┬──┘    └──┬──┘         └──┬──┘
     │              │          │          │               │
     │           ┌──▼──┐       │       ┌──▼──┐            │
     │           │ 03  │◄──────┘       │     │            │
     │           │Cache│               │     │            │
     │           └─────┘               │     │            │
     │                                 │     │            │
     └─────────────────────────────────┴─────┴────────────┘
                               │
                               ▼
                  ┌──────── Cross-cutting concerns ────────┐
                  │                            │
              ┌───▼───┐  ┌───▼───┐  ┌─────▼──┐  ┌───▼───┐
              │  06   │  │  09   │  │  10    │  │  08   │
              │Config │  │Storage│  │ Sec    │  │ MELT  │
              └───┬───┘  └───┬───┘  └────┬───┘  └───┬───┘
                  │          │           │          │
                  └──────────┼───────────┴──────────┘
                             │
                       ┌─────▼─────┐
                       │  11 + 13   │
                       │  Perf+Ops  │
                       └────────────┘
                                
                       ┌────────────┐
                       │     07     │ ← consumes 04 §12 TrajectoryEvent
                       │ Deploy/UI  │
                       └────────────┘
```

**Core reading order**: 04 is the central hub — understand it first, and the rest of the documents serve as expansions of it.

---

## 7. Glossary

In alphabetical order:

| Term | Meaning | Source |
|---|---|---|
| **Agent** | Executable unit that takes input and produces output, may invoke LLM/Tool | Doc 04 §4 |
| **AgentEvent** | Internal event-sourcing record (≠ TrajectoryEvent) | Doc 04 §3.2 |
| **Audit Log** | Tamper-evident compliance record (≠ MELT) | Doc 06 §10 |
| **BYOK** | Bring Your Own Key — user supplies their own LLM API key | Doc 07 §3.1 |
| **Cache Key** | Content-addressed hash including tenant + IAM + model + content | Doc 03 §3.2 |
| **Capability** | Provider capability descriptor (supports tool use / structured output etc.) | Doc 01 §5 |
| **CLI Subprocess** | Long-lived `claude`/`gemini` CLI process, reused across requests | Doc 01 §6.2 |
| **Compensation** | Inverse operation — rolls back side effects under the Saga pattern | Doc 04 §6 |
| **Content Store** | Storage for large payloads, indirected via ContentRef | Doc 04 §3.3 |
| **Critic** | Agent that reviews Worker output in an independent round | Doc 04 §2.1 |
| **Dynamic Suffix** | The portion of the prompt that changes per request — never enters the cache key | Doc 03 §10.5 |
| **Effective Config** | Final config after merging the 5 layers | Doc 06 §2 |
| **Event Sourcing** | Event append as the sole source of truth | Doc 04 §3.2 |
| **FFI** | Foreign Function Interface — direct Rust ↔ Python/Node bindings | Doc 12 §6-7 |
| **Frontend Adapter** | UI layer that consumes the TrajectoryEvent stream | Doc 07 §4 |
| **Idempotency Key** | Idempotency key — deduplication on replay/retry | Doc 04 §7 + Doc 05 §4.3 |
| **L1/L2/L3 Cache** | In-process / Redis / Provider explicit — three-tier cache | Doc 03 §2 |
| **MELT** | Metrics/Events/Logs/Traces — the four pillars of observability | Doc 08 |
| **MCP** | Model Context Protocol — Tool protocol proposed by Anthropic | Doc 05 §5 |
| **Middleware** | Tower-style onion layer handling cross-cutting concerns | Doc 02 |
| **ModelHint** | Abstract model selection (Tier / Explicit / Ensemble) | Doc 01 §4.1 |
| **Orchestrator** | Agent that does no reasoning, only decomposes the task DAG | Doc 04 §2.1 |
| **PII** | Personally Identifiable Information | Doc 08 §11 + Doc 10 §8 |
| **Pipeline** | Request handling chain composed of Middleware | Doc 02 |
| **Principal** | Caller identity (user / service account / subprocess) | Doc 10 §4 |
| **PromptBuilder** | Three-stage prompt assembler | Doc 04 §11 |
| **Provider** | LLM backend abstraction (API / CLI / embedded) | Doc 01 |
| **RequestContext** | Request-scoped context containing trace/tenant/principal/cancel/budget | Doc 02 §3.3 |
| **Routing Policy** | Provider selection policy (Tier / Cost / Latency / Fallback) | Doc 01 §12 |
| **SLI / SLO** | Service Level Indicator / Objective | Doc 11 §2 |
| **SaaS / Self-Hosted / Local-First / Hybrid** | The 4 deployment shapes | Doc 07 §2 |
| **SecretRef** | Secret reference (vault path / env var name) | Doc 06 §5 |
| **Session** | User session, many-to-one with trajectory | Doc 06 §3.3 |
| **Side Effect Kind** | Pure / Isolated / Reversible / Irreversible — four levels | Doc 04 §4.4 |
| **Singleflight** | Coalescing of concurrent same-key requests | Doc 03 §6 |
| **Skill** | Composite capability, may include multi-step LLM + Tool orchestration | Doc 05 §6 |
| **Static Prefix** | The portion of the prompt stable on a monthly scale — primary reuse target for L3 cache | Doc 03 §10.5 |
| **Tenant** | Hard isolation boundary — core security unit | Doc 06 §3 |
| **TaskBudget** | Task-level budget envelope (token/cost/duration/hops/replans) | Doc 04 §8 |
| **Tool** | Atomic function, invoked by LLM via tool_use | Doc 05 §3 |
| **Trajectory** | Execution trace of a single task — branchable, abandonable | Doc 04 §3.1 |
| **TrajectoryEvent** | Business event exposed to the Frontend (≠ AgentEvent) | Doc 04 §12 |
| **TUI** | Terminal UI (ratatui-based) | Doc 07 §6 |
| **TTFT** | Time To First Token — LLM first-token latency | Doc 11 §2.1 |

---

## 8. Implementation Status

> This section is dynamic and updated continuously as implementation progresses.

### 8.1 Current Status (2026-06-10)

```
[██████████████░░░░░░] ~70%

Done:
- ✅ 22 design documents (00-21)
- ✅ 14 crates in Cargo workspace (~158 .rs files in crates/*/src)
- ✅ Core trait definitions (LlmProvider, LlmService, Middleware,
                            BudgetStore, BatchSubmitter, Tool, Agent, …)
- ✅ 7 HTTP provider backends (openai / anthropic / gemini / deepseek /
                               vllm / mlx / llamacpp)
- ✅ 3 subscription CLI backends (claude_cli / gemini_cli / codex_cli)
- ✅ MockProvider for tests (incl. per-call response queue)
- ✅ Pipeline middleware stack:
     telemetry, cache, per-call/tenant budget, fallback, retry,
     routing, circuit_breaker, event_emitter, validation
- ✅ Cache registry (L1 in-memory + L2 SQLite)
- ✅ Agent runtime: Trajectory + AgentEvent + Orchestrator/Worker/Critic
- ✅ Agent abstraction (tars-model): trait Agent + Task/SkillSet/
     Permissions; TarsAgent (LLM-backed) + EnsembleAgent (task hedge)
- ✅ Tool registry + builtins (read/list_dir/write_file/edit_file/bash,
     cwd-scoped) + permission enforcement at dispatch
- ✅ Pipeline event store (SQLite events + CAS bodies.db)
- ✅ Output validators framework + 4 builtins
- ✅ Cost & reliability features (roadmap.md §1-§5)
- ✅ Batch mode (Anthropic + OpenAI; Gemini deferred stub)
- ✅ Rust-side convenience APIs: Pipeline::default_chain,
     shared_runtime, complete_sync (commit 7ef7f34)
- ✅ tars-cli with run/plan/run-task/probe/bench/trajectory/events/init
- ✅ tars-py PyO3 wheel + Python API surface
- ✅ tars-node napi-rs native addon (Node / TypeScript bindings)
- ✅ tars-server: personal-mode HTTP/REST shell (complete + streaming)
- ✅ User-facing docs: USER-GUIDE, observability, providers/,
     recipes/, roadmap

Partial:
- ⏳ M4: tools registry done; MCP stdio support NOT yet
- ⏳ M5: rich tars-cli done; TUI NOT yet
- ⏳ M7: personal-mode HTTP server (tars-server) done; SPA dashboard
       + multi-tenant control plane NOT yet
- ⏳ M9: telemetry middleware done; OTel exporter composable
       but not wired in CLI; load testing NOT yet

Not started:
- ⬜ M6: Postgres schema, IAM engine, Team mode
- ⬜ Voice / Realtime transport (Modality::Audio reserved, 0 runtime)
- ⬜ LLM-as-judge eval runner (EvaluationScored schema exists;
                              see Doc 16)
```

**Reality check vs the v1.0 target**: M0-M3 are substantively complete
(and then some — 7 HTTP backends instead of 1). M8 (FFI bindings) is now
complete (PyO3 + napi-rs). M4-M5 and M7 (personal-mode server done, SPA +
multi-tenant control plane pending) are partial. M6 is the largest
remaining chunk before v1.0; M9 is partial.

A lot of work that wasn't in the original milestone list also landed:
output validators (Doc 15), pipeline event store (Doc 17), the three
subscription CLI backends, the entire cost & reliability roadmap, and
batch mode — all visible in `docs/roadmap.md` and `docs/recipes/`.

### 8.2 Implementation Milestones (status by milestone)

| # | Milestone | Status | Notes |
|---|---|---|---|
| **M0** | Foundation | ✅ | tars-types / config / storage / melt all shipped |
| **M1** | Single Provider, Single Path | ✅ | Shipped 7 HTTP backends, not just 1 |
| **M2** | Multi-Provider + Routing | ✅ | StaticPolicy/TierPolicy + CircuitBreaker + ErrorClass |
| **M3** | Agent Runtime Core | ✅ | Trajectory + AgentEvent + Worker + Critic in tars-runtime |
| **M4** | Tools + MCP | ⚠️ partial | Registry + builtins done; **MCP stdio NOT yet** |
| **M5** | CLI + TUI | ⚠️ partial | Rich `tars` CLI shipped; **TUI NOT yet** |
| **M6** | Multi-tenant + Postgres | ❌ | Only SQLite; IAM enforcement NOT yet |
| **M7** | Web Dashboard | ⚠️ partial | Personal-mode HTTP server (tars-server) ✅ / SPA + tonic NOT yet |
| **M8** | FFI Bindings | ✅ | PyO3 ✅ / napi-rs ✅ |
| **M9** | Production Readiness | ⚠️ partial | Telemetry + event store ✅ / OTel exporter composable / load test ❌ |

**Above and beyond the milestones** (added as need emerged, all shipped):

| Item | Where |
|---|---|
| Output validator framework | Doc 15, tars-pipeline::validation |
| Pipeline event store (per-call observability DB) | Doc 17, tars-storage::sqlite |
| 3 subscription CLI providers | tars-provider::backends::{claude_cli,gemini_cli,codex_cli} |
| Cost & reliability middlewares (budget × 2, fallback, max_wait) | `docs/roadmap.md` |
| Batch mode (Anthropic + OpenAI) | `docs/recipes/batch.md` |
| User-facing docs subtree (USER-GUIDE, observability, providers/, recipes/) | `docs/` |
| Rust-side convenience APIs (`default_chain`, `shared_runtime`, `complete_sync`) | tars-pipeline + tars-runtime |

### 8.3 Open / forward-looking planning docs

Two living roadmap docs supplement the milestone view above:

- [`docs/roadmap.md`](../roadmap.md) — **closed for v1**. The cost &
  reliability features (§1-§5). All shipped except the Gemini batch
  real impl. The "What's next" section there enumerates the bigger
  gaps (voice, eval, RAG recipe, built-in tools).
Anything not in the roadmap and not in §8.1's "Done" list is, as
far as the project is concerned, **not yet planned**.

### 8.4 Out of Scope for v1.0

- WASM binding
- Hybrid deployment mode (cloud control plane)
- Full SaaS multi-region deployment
- AI-assisted incident analysis
- gRPC server (HTTP first)

Deferred to v2.0 or as needed.

---

## 9. Contribution Guide

### 9.1 Doc Maintenance

- Any architectural change updates the corresponding doc first, then the code
- Cross-doc references must use relative paths `./XX-name.md#section-id`
- The anti-pattern checklist is a treasure — pitfalls hit must be added in
- The TODO and open-questions sections are the backlog; clean up completed items at review

### 9.2 Code Contribution

- Comply with the trait contract of the corresponding doc
- New features must have tests (unit + conformance)
- Performance-critical paths must have benchmarks
- Any security / isolation-related change requires 2-person review

### 9.3 Doc Evolution

- Adding/removing docs requires team discussion
- Schema field additions/removals follow the §11 versioning process
- Reading paths (§5) updated as the team grows

### 9.4 Reporting Issues

- Doc unclear → GitHub Issue + label `docs`
- Design questions → Discussions tab
- Security issues → privately email security@tars.dev (see Doc 10 §15.1)

---

## 10. History and Versions

| Version | Date | Changes |
|---|---|---|
| 0.1 | 2026-05 | 13 design documents complete; implementation not yet started |

---

## 11. Acknowledgements and References

Projects and papers that informed / inspired the design (alphabetical):

- **Anthropic Claude Code** — long-lived CLI pattern + JSONL bidirectional protocol
- **Apache Cassandra** — multi-tenant partitioning model
- **HashiCorp Vault** — Secret namespacing design
- **HEARSAY-II / Blackboard Architecture** — classical model for multi-Agent collaboration
- **LangGraph** — Cyclic state machine for agents (though we ultimately went with event sourcing)
- **OpenAI / Anthropic / Google API docs** — concrete semantics of Tool calling / Structured Output / Caching
- **OpenTelemetry** — full-stack observability standard
- **PostgreSQL pg_partman** — automated time-based partitioning
- **Saga Pattern (CIDR 1987)** — distributed transaction compensation
- **Temporal / Restate / Cadence** — Durable workflow inspirations
- **Tower (Rust) / Axum** — Middleware as Layer pattern
- **vLLM / mistral.rs** — Rust LLM inference ecosystem

---

## 12. Contact

`<TBD>` (to be filled in once the project is formally launched)
