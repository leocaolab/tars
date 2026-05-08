# Doc 04 — Agent Runtime and Trajectory Tree

> Scope: defines the runtime abstractions for multi-Agent coordination — topology, state model, message contract, backtrack and recovery mechanisms, Frontend contract.
>
> Upstream (consumers): CI Mode / TUI Mode / Web Dashboard and other Frontend Adapters, see Doc 07.
>
> Downstream (dependencies): Doc 02 Middleware Pipeline → Doc 01 LlmProvider; Doc 03 Cache Registry; Doc 05 Tool/MCP (TBD).

---

## 1. Design Goals

| Goal | Description |
|---|---|
| **DAG is the plan, not the runtime** | Execution shape is a state machine with backtrack, loops, and abandonment. Plan as DAG, execute as event-sourced trajectory tree |
| **Crash recovery** | After a process dies and restarts, tasks resume from the last checkpoint without losing completed work |
| **Backtrack safety** | Any executed side effect must be compensable (or designed not to occur in intermediate steps) |
| **Hard budget constraints** | Each task has hard upper bounds on tokens / duration / agent hops / replan count; abort directly when hit, no runaway allowed |
| **Frontend agnostic** | Runtime emits only a `TrajectoryEvent` stream; it knows nothing about CI / TUI / Web |
| **Strong contract for multi-Agent** | Inter-Agent messages go through Rust enum + provider strict structured output; raw text exchange is forbidden |
| **Observable and replayable** | Complete event log + LLM response capture; any historical task can be reproduced in a test environment |

**Anti-goals**:
- No free mesh / P2P agent communication (O(n²) complexity, undebuggable)
- Don't let LLMs decide routing / task completion / tool selection (these must be deterministic decisions in code)
- No prompt template rendering at the Runtime layer (that's the Prompt Builder's responsibility, see §4.5)
- No specific Frontend embedded (trace display, conversation UI, Markdown report generation are Adapter concerns)

---

## 2. Topology: Hierarchical Orchestration + DAG Workers + Critic

### 2.1 Mandatory structure

```
                    ┌────────────────────────┐
                    │  Orchestrator (Planner) │
                    │  L1 model + strict JSON │
                    └────────────┬───────────┘
                                 │ emit task DAG
                ┌────────────────┼────────────────┐
                ▼                ▼                ▼
         ┌──────────┐     ┌──────────┐     ┌──────────┐
         │ Worker A │     │ Worker B │     │ Worker C │  ← parallelizable leaves
         │ L2 / L2' │     │ L2 / L2' │     │ L2 / L2' │
         └────┬─────┘     └────┬─────┘     └────┬─────┘
              └────────────────┼────────────────┘
                               ▼
                    ┌──────────────────────┐
                    │  Aggregator          │
                    │  pure code, no LLM   │
                    └──────────┬───────────┘
                               ▼
                    ┌──────────────────────┐
                    │  Critic (separate    │
                    │  round, L3 / Worker- │
                    │  tier model)         │
                    └──────────┬───────────┘
                               │
                  ┌────────────┴────────────┐
                  ▼                         ▼
            accept (commit phase)    replan (fork from parent)
```

### 2.2 Mandatory rules

1. **Orchestrator does no reasoning** — its output is always a structured task DAG (schema-constrained). Work that requires thinking is dispatched to Workers.
2. **Workers do not communicate with each other directly** — all collaboration goes through the Context Store's append-only event stream (§6).
3. **Critic is a separate round** — it does not reuse a Worker's conversational context, avoiding contamination by reasoning paths.
4. **Aggregator is pure code** — no LLM calls; performs deterministic operations like schema concatenation, deduplication, sorting.
5. **mesh / P2P communication is forbidden** — any "Agent X directly calls Agent Y" is rejected at compile time.

### 2.3 Why mesh is rejected

Failure modes of mesh topology (early autogen, CrewAI free mode):
- Message complexity O(n²)
- Token cost grows exponentially (every message has to carry all preceding messages)
- Debugging impossible (which Agent's which utterance caused the final bug?)
- Budgeting impossible (cycles cannot be statically analyzed)

Production-grade multi-Agent systems eventually all converge to the hierarchical orchestrator + structured DAG + critic loop shape. LangGraph, OpenAI Swarm, CrewAI's "hierarchical mode" are all this shape.

---

## 3. Core Data Model

### 3.1 Trajectory Tree

```rust
pub struct Trajectory {
    pub id: TrajectoryId,
    pub root_task: TaskId,
    pub parent: Option<TrajectoryId>,         // forms a tree
    pub branch_reason: BranchReason,           // why this branched off from parent
    pub status: TrajectoryStatus,
    pub head_state: StateRef,                  // points to offset in event log
    pub pending_compensations: Vec<CompensationAction>,
    pub budget_remaining: TaskBudget,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

pub enum TrajectoryStatus {
    Active,
    Suspended { reason: SuspendReason },
    Completed { result: TaskResult },
    Dead { cause: DeathCause },           // marked dead by backtrack
}

pub enum BranchReason {
    Root,                                    // task origin
    Replan { from: TrajectoryId, critic_feedback: String },
    Fork { from: TrajectoryId, hypothesis: String },     // tree-of-thoughts style
    Recovery { from: TrajectoryId, error: ErrorRef },    // new branch on crash recovery
}

pub enum DeathCause {
    BacktrackedAfterCriticReject,
    BacktrackedAfterError(ErrorRef),
    BudgetExhausted,
    DeadlineExceeded,
    ExplicitAbort,
}
```

### 3.2 Event log (single source of truth)

```rust
pub enum AgentEvent {
    /// Task birth
    TaskCreated { task_id: TaskId, spec: TaskSpec, principal: Principal },
    
    /// Trajectory lifecycle
    TrajectoryStarted { traj: TrajectoryId, parent: Option<TrajectoryId>, reason: BranchReason },
    TrajectorySuspended { traj: TrajectoryId, reason: SuspendReason },
    TrajectoryResumed { traj: TrajectoryId, by: ResumeTrigger },
    TrajectoryAbandoned { traj: TrajectoryId, cause: DeathCause },
    
    /// Agent step
    StepStarted { 
        traj: TrajectoryId, 
        step_seq: u32,
        agent: AgentId, 
        idempotency_key: String,
        input_ref: ContentRef,           // large inputs go to ContentStore; events hold only refs
    },
    StepCompleted { 
        traj: TrajectoryId,
        step_seq: u32,
        output_ref: ContentRef,
        side_effects: Vec<SideEffectRef>,
        usage: Usage,
    },
    StepFailed { 
        traj: TrajectoryId,
        step_seq: u32,
        error: AgentError, 
        classification: ErrorClass,
    },
    
    /// Side-effect compensation
    CompensationExecuted { 
        traj: TrajectoryId,
        compensation: CompensationRef,
        result: CompensationResult,
    },
    
    /// LLM raw response capture (required for replay)
    LlmResponseCaptured { 
        traj: TrajectoryId,
        step_seq: u32,
        provider: ProviderId,
        raw_response: ContentRef,
    },
    
    /// Checkpoint (reduces replay cost)
    Checkpoint { 
        traj: TrajectoryId,
        step_seq: u32,
        state_snapshot: ContentRef,
    },
}
```

**Key invariants**:
1. **Events are append-only, never mutated** — all "corrections" are realized by appending new events (e.g., `TrajectoryAbandoned` marks a trajectory as dead).
2. **Large payloads go through ContentStore + ContentRef** — events themselves stay small (<4KB), enabling chronological scans and persistence.
3. **`idempotency_key` is fixed at StepStarted** — format `hash(traj_id + step_seq + input_ref)`; all external calls (LLM, tool, DB) carry this key for dedup on replay.
4. **`LlmResponseCaptured` and `StepCompleted` are two separate events** — the former preserves raw response for replay, the latter stores the parsed/aggregated final output. This way, even if parsing logic changes later, replay can re-parse from the old raw response.

### 3.3 ContentStore: decoupling large payload from events

```rust
#[async_trait]
pub trait ContentStore: Send + Sync {
    async fn put(&self, content: &[u8]) -> Result<ContentRef, StoreError>;
    async fn get(&self, refr: &ContentRef) -> Result<Vec<u8>, StoreError>;
    async fn delete(&self, refr: &ContentRef) -> Result<(), StoreError>;
}

pub struct ContentRef {
    pub hash: [u8; 32],              // SHA-256 content addressing, automatic dedup
    pub size: u64,
    pub mime: String,
    pub backend: ContentBackend,     // Postgres bytea / S3 / FS
}
```

Event log lives in Postgres; ContentStore can be the same DB (small content as bytea) or S3 / local FS (large content like full LLM responses, code review reports).

---

## 4. Agent Abstraction and Message Contract

### 4.1 Agent trait

```rust
#[async_trait]
pub trait Agent: Send + Sync {
    fn id(&self) -> &AgentId;
    fn role(&self) -> AgentRole;
    
    /// Single-step execution: pure function (state, input) → (output, side_effects)
    async fn execute(
        &self,
        ctx: AgentContext,
        input: AgentInput,
    ) -> Result<AgentStepResult, AgentError>;
    
    /// Declares the side-effect kinds this Agent may produce.
    /// Runtime prepares compensation capabilities based on this list.
    fn declared_side_effects(&self) -> &[SideEffectKind];
}

pub struct AgentContext {
    pub trajectory: TrajectoryId,
    pub step_seq: u32,
    pub task_budget: BudgetView,             // remaining budget
    pub principal: Principal,
    pub deadline: Option<Instant>,
    pub cancel: CancellationToken,
    pub llm_service: Arc<dyn LlmService>,    // pipeline from Doc 02
    pub context_store: Arc<dyn ContextStore>,
    pub tool_registry: Arc<dyn ToolRegistry>, // Doc 05
}

pub enum AgentRole {
    Orchestrator,
    Worker { domain: String },               // "code_review" / "security_audit" / ...
    Critic,
    Aggregator,                              // pure-code Agent, no LLM calls
}

pub struct AgentStepResult {
    pub output: AgentMessage,
    pub side_effects: Vec<SideEffect>,
    pub usage: Usage,
    pub raw_llm_response: Option<ContentRef>,  // for replay
}
```

### 4.2 AgentMessage (strongly-typed message contract)

All inter-Agent communication goes through one enum. **Raw text exchange is forbidden**:

```rust
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum AgentMessage {
    /// Orchestrator dispatches a task
    PlanIssued {
        plan_id: PlanId,
        nodes: Vec<PlanNode>,
        edges: Vec<PlanEdge>,
    },
    
    /// Worker reports an intermediate artifact
    PartialResult {
        agent: AgentId,
        artifact: Artifact,
        confidence: f32,
    },
    
    /// Worker needs more context
    NeedsClarification {
        agent: AgentId,
        question: String,
        blocking: bool,
    },
    
    /// Critic verdict
    CriticVerdict {
        verdict: Verdict,
        rejected_reasons: Vec<String>,
        replan_hints: Vec<String>,
    },
    
    /// Failure report
    Failed {
        agent: AgentId,
        error: AgentError,
        recoverable: bool,
    },
    
    /// Task completion
    Completed {
        final_artifact: Artifact,
        total_usage: Usage,
    },
}
```

**LLM integration**: when each Agent calls LlmService, `req.structured_output = Some(AgentMessage::json_schema_for_role(self.role()))`, letting provider-side strict mode emit an `AgentMessage` substructure directly. The Rust side does `serde_json::from_str` for one-shot deserialization, with zero string concatenation / regex extraction.

### 4.3 Model tiering (don't let the LLM route itself)

```rust
impl Agent for OrchestratorAgent {
    async fn execute(&self, ctx: AgentContext, input: AgentInput) -> ... {
        let req = ChatRequest {
            model: ModelHint::Tier(ModelTier::Default),  // L1 workhorse, requires stable JSON
            structured_output: Some(AgentMessage::schema_for(AgentRole::Orchestrator)),
            ...
        };
        // ...
    }
}

impl Agent for ReasoningWorker {
    fn execute(&self, ctx: AgentContext, input: AgentInput) -> ... {
        let req = ChatRequest {
            model: ModelHint::Tier(ModelTier::Reasoning),  // L2 top-tier model
            ...
        };
    }
}

impl Agent for FastClassifierWorker {
    // No LLM; calls Doc 01's ClassifierProvider (DeBERTa) directly
}
```

Four-tier allocation:
- L1 (Orchestrator / tool selection): stable JSON, mid-cost (gpt-4o-mini / claude-haiku)
- L2 (Reasoning Worker): reasoning workhorse (claude-opus / o1 / gemini-pro)
- L2' (Specialized Worker): domain-optimized (qwen-coder for code / gemini-flash for long docs)
- L3 (Critic): same tier as Worker or half a tier lower
- L4 (routing / extraction / classification): local small models (DeBERTa / qwen-0.5B / bge-small)

### 4.4 Side-effect classification

```rust
pub enum SideEffectKind {
    /// Pure compute, no external side effects (LLM inference itself: costs money but is semantically reproducible)
    PureCompute,
    
    /// Writes in an isolated environment (temp dir / shadow branch / staging table)
    /// Auto-compensation: delete/rollback
    IsolatedWrite { resource: ResourceRef, undo: UndoAction },
    
    /// Creates reversible cloud resources (cache handle / temp bucket)
    /// Explicit compensation: call delete API
    ReversibleResource { resource: ResourceRef, cleanup: CleanupAction },
    
    /// Irreversible external action (git push / send email / payment / delete production data)
    /// Must be deferred to Commit Phase; not allowed in intermediate steps
    Irreversible { resource: ResourceRef },
}

pub struct SideEffect {
    pub kind: SideEffectKind,
    pub idempotency_key: String,          // dedup on replay
    pub created_at: SystemTime,
}
```

**Mandatory constraint**: After each StepCompleted, Runtime scans side_effects; if it finds an `Irreversible` outside of a Commit Phase, it **panics / rejects the step directly**. This is an engineering-discipline-level hard constraint, not LLM self-restraint.

---

## 5. Context Store: append-only shared context

```rust
#[async_trait]
pub trait ContextStore: Send + Sync {
    /// Append an event after an Agent finishes
    async fn append(&self, event: AgentEvent) -> Result<EventOffset, StoreError>;
    
    /// Pull upstream outputs (filtered by trajectory + time window / step range)
    async fn fetch_for_agent(
        &self,
        traj: TrajectoryId,
        agent_input_spec: &AgentInputSpec,    // event types this Agent is interested in
    ) -> Result<Vec<AgentEvent>, StoreError>;
    
    /// Streaming subscription (for Frontend Adapters)
    async fn subscribe(
        &self,
        traj_filter: TrajectoryFilter,
    ) -> Result<BoxStream<'static, AgentEvent>, StoreError>;
    
    /// Replay: replay in chronological order from a given offset
    async fn replay_from(
        &self,
        traj: TrajectoryId,
        from_offset: EventOffset,
    ) -> Result<BoxStream<'static, AgentEvent>, StoreError>;
}
```

### 5.1 Context Compactor

As Agent count grows, the raw event stream grows exponentially — you can't shove all raw events into the next Agent. Insert a compaction stage:

```rust
pub trait ContextCompactor: Send + Sync {
    /// Compact a raw event stream into the minimum necessary context for the next Agent
    async fn compact(
        &self,
        events: Vec<AgentEvent>,
        target_agent: &AgentId,
        max_tokens: u32,
    ) -> Result<CompactedContext, CompactorError>;
}
```

Implementation strategies:
- **Schema-aware**: keep only the event types the target agent has declared interest in
- **Recency window**: by default, only the most recent N events
- **LLM compression (fallback)**: if the above two still exceed max_tokens, use an L4 model for summarization

**Key**: Compaction itself is deterministic (schema/window mode); LLM summarization is a degraded fallback. Otherwise compaction introduces non-determinism and replay isn't reproducible.

### 5.2 Persistence shape

```
Postgres:
  agent_events     (id BIGSERIAL, traj_id UUID, event JSONB, created_at TIMESTAMPTZ)
  trajectories    (id UUID, parent UUID, status, budget_remaining JSONB, ...)
  content_refs    (hash BYTEA PRIMARY KEY, size, mime, backend, payload BYTEA NULL)
  
S3 / FS (large content):
  content/{first2}/{remaining30}.bin    -- ContentRef content addressing
```

Event log is partitioned by `traj_id` and indexed by `created_at`. After 30 days, archive to cold storage (S3 Glacier).

---

## 6. Backtrack: Saga Compensation

### 6.1 Trigger scenarios

1. **Critic rejects**: fork from parent, inject critic feedback as a replan hint
2. **Step fails (permanent error)**: trajectory dies + compensation + parent takes over
3. **Agent stuck (timeout)**: same as above
4. **Budget exhausted (remaining < current step estimate)**: suspend, no compensation (preserve intermediate artifacts)

### 6.2 Backtrack flow

```rust
impl Runtime {
    async fn backtrack(
        &self,
        traj: TrajectoryId,
        target: BacktrackTarget,
    ) -> Result<TrajectoryId, RuntimeError> {
        let trajectory = self.load_trajectory(traj).await?;
        
        // 1. Mark current trajectory dead (blocks any concurrent writes)
        self.event_store.append(AgentEvent::TrajectoryAbandoned { 
            traj, 
            cause: target.death_cause() 
        }).await?;
        
        // 2. Run compensations in reverse
        for comp in trajectory.pending_compensations.iter().rev() {
            match comp.execute().await {
                Ok(_) => {
                    self.event_store.append(AgentEvent::CompensationExecuted {
                        traj,
                        compensation: comp.id.clone(),
                        result: CompensationResult::Success,
                    }).await?;
                }
                Err(e) if e.already_undone() => continue,  // idempotent, skip
                Err(e) => {
                    // Compensation failure = serious incident, escalate to human
                    self.event_store.append(AgentEvent::CompensationExecuted {
                        traj,
                        compensation: comp.id.clone(),
                        result: CompensationResult::Failed(e.to_string()),
                    }).await?;
                    self.escalate_to_human(traj, &comp, &e).await?;
                    return Err(RuntimeError::CompensationFailed(e));
                }
            }
        }
        
        // 3. Find fork point
        let resume_state = match target {
            BacktrackTarget::ParentTrajectory => trajectory.parent_state(),
            BacktrackTarget::Checkpoint(cp_id) => self.load_checkpoint(cp_id).await?,
        };
        
        // 4. Open a new trajectory, feed hint to planner
        let new_traj = self.fork_trajectory(
            resume_state,
            BranchReason::Replan {
                from: traj,
                critic_feedback: target.feedback().to_string(),
            },
        ).await?;
        
        Ok(new_traj)
    }
}
```

### 6.3 Handling compensation failure

Compensation failure is more terrifying than execution failure — it means the system has entered an inconsistent state (partial side effects remain). Handling flow:

1. Immediately freeze related resources (resource access short-circuits to 503, avoiding new decisions on inconsistent state)
2. Escalate to oncall (PagerDuty / Slack alert)
3. Queue the failed compensation onto the "manual handling" queue
4. Suspend everything downstream of this trajectory

Never allow "compensation failed but the program keeps running."

### 6.4 Backtrack depth ceiling

```rust
pub struct BacktrackPolicy {
    pub max_backtrack_depth: u8,         // default 3
    pub max_replans_per_task: u8,        // default 2
}
```

In actual production:
- > 80% of backtracks only go back 1-2 steps
- backtracks > 5 steps usually mean the task definition is broken; the right move is to abandon the whole task + notify a human

Above the ceiling, suspend directly; don't force continuation.

---

## 7. Recovery: Replay + Checkpoint + Idempotency

### 7.1 Process crash recovery

On startup, scan all `Active` / `Suspended` trajectories; for each:

```rust
async fn recover_trajectory(&self, traj: TrajectoryId) -> Result<(), RuntimeError> {
    // 1. Find latest checkpoint to avoid replaying from scratch
    let checkpoint = self.event_store.find_latest_checkpoint(traj).await?;
    let mut state = checkpoint.map(|c| self.load_state(&c.state_snapshot)).await?
        .unwrap_or_else(State::initial);
    
    // 2. Replay events after the checkpoint
    let events = self.event_store.events_since(traj, checkpoint.offset).await?;
    let mut in_flight: Option<InFlightStep> = None;
    
    for event in events {
        match &event {
            AgentEvent::StepStarted { step_seq, agent, idempotency_key, .. } => {
                in_flight = Some(InFlightStep {
                    step_seq: *step_seq,
                    agent: agent.clone(),
                    idempotency_key: idempotency_key.clone(),
                });
            }
            AgentEvent::StepCompleted { .. } | AgentEvent::StepFailed { .. } => {
                in_flight = None;
            }
            _ => {}
        }
        state.apply(event);
    }
    
    // 3. If there's an in-flight step (executing at crash time), retrigger via idempotency_key
    //    LLM calls hit cache via prompt hash (Doc 03); tool calls dedup via idempotency_key
    if let Some(step) = in_flight {
        self.retry_step_with_idempotency(traj, step).await?;
    }
    
    // 4. Resume normal scheduling
    self.resume_normal_execution(traj, state).await
}
```

**Key invariants**:
- `idempotency_key = hash(traj_id + step_seq + input_ref)` — stable, consistent across replay
- All external calls (LlmService, ToolRegistry, ContextStore) must accept and respect the idempotency_key
- LLM calls dedup naturally via Doc 03 cache (same prompt hash hits)

### 7.2 The non-determinism trap in replay

LLM calls with `temperature > 0` are non-deterministic; the replayed token stream diverges from the original → state diverges. Two solutions:

**Option A (default)**: Force determinization of all LLM responses through the cache
- First call: cache miss + real generation + cache write
- Replay: cache hit, identical response

**Option B (more thorough)**: Persist LLM raw output as events
- `AgentEvent::LlmResponseCaptured` is appended after every LLM call
- During replay, detect this event, skip the real LLM call, read raw response from the event
- This is the orthodox approach in Temporal / Cadence
- Cost: event log bloat (each response can be tens of KB)

We adopt **Option A as the main path + Option B as backup**: cache is the regular path, LlmResponseCaptured is enabled only at trace level (debug / regulatory audit scenarios).

### 7.3 Checkpoint policy

```rust
pub struct CheckpointPolicy {
    pub strategy: CheckpointStrategy,
}

pub enum CheckpointStrategy {
    /// Fixed: one checkpoint every N steps
    EveryNSteps(u32),
    
    /// Cost-weighted: must checkpoint after LLM calls; skip cheap local steps
    Cost { llm_step_must_checkpoint: bool, local_step_threshold_ms: u64 },
    
    /// At trajectory fork points (before the fork)
    AtForkPoints,
    
    /// Combination
    Combined(Vec<CheckpointStrategy>),
}
```

Default policy: `Combined([EveryNSteps(5), AtForkPoints, Cost { llm_step_must_checkpoint: true, ... }])`

Rationale: LLM calls are the largest recovery cost (money + time); after checkpointing, a crash only requires replaying subsequent cheap steps.

### 7.4 Suspend / Resume

Not every "incomplete" is a failure — some are normal waits:

```rust
pub enum SuspendReason {
    BudgetExhausted { 
        spent: Usage, 
        partial_result: Option<Artifact>,
    },
    AwaitingHumanApproval { 
        question: String, 
        choices: Vec<Choice>,
    },
    AwaitingExternalEvent { 
        event_type: String, 
        correlation_id: String,
    },
    RateLimited { 
        retry_after: Duration,
    },
}
```

Resume is an explicit API:

```rust
async fn resume(
    &self,
    traj: TrajectoryId,
    trigger: ResumeTrigger,
) -> Result<(), RuntimeError>;

pub enum ResumeTrigger {
    HumanApproval { decision: Choice },
    ExternalEventReceived { payload: serde_json::Value },
    BudgetIncreased { new_budget: TaskBudget },
    Manual,
}
```

---

## 8. Budget Envelope (TaskBudget)

```rust
pub struct TaskBudget {
    pub max_tokens_total: u64,        // summed across all Agents in the task
    pub max_cost_usd: f64,             // hard USD ceiling
    pub max_wall_clock: Duration,      // wall-clock time
    pub max_agent_hops: u8,            // max Agent invocations, typically ≤10
    pub max_replans: u8,               // Critic-triggered replan count, typically ≤2
    pub max_backtrack_depth: u8,       // backtrack depth per task
    pub fallback: FallbackStrategy,
}

pub enum FallbackStrategy {
    /// Fail outright
    Fail,
    
    /// Use the last valid intermediate artifact + L1 model to generate a summary
    BestEffortSummary,
    
    /// Suspend, notify human
    SuspendForHuman,
}
```

Decremented in Middleware before each Agent invocation; on overshoot, abort the current step + trigger fallback.

---

## 9. Cancel Propagation

Continuing the chain from Doc 02 §5, Runtime is one of the cancel originators:

```
Frontend user presses Ctrl+C / clicks "Stop"
       │
       ▼
Frontend Adapter calls runtime.cancel_task(task_id)
       │
       ▼
Runtime sets cancel_token on all active trajectories of the task
       │
       ▼
Executing Agents listen on ctx.cancel.cancelled() → return early
       │
       ▼
LlmService stream Drop → CLI subprocess interrupt per Doc 01 §6.2.1
```

Each Agent must cooperate during long operations (LLM calls, tool calls) by listening for cancel via `tokio::select!`:

```rust
async fn execute(&self, ctx: AgentContext, input: AgentInput) -> Result<...> {
    tokio::select! {
        result = self.do_work(&ctx, input) => result,
        _ = ctx.cancel.cancelled() => Err(AgentError::Cancelled),
    }
}
```

---

## 10. Integration with the Pipeline

Agent Runtime is a **consumer** of the Pipeline, not a wrapper. Each Agent calls the Pipeline through `AgentContext::llm_service`, getting LLM capability that has already been processed by IAM / Cache / Guard / Routing:

```rust
async fn execute(&self, ctx: AgentContext, input: AgentInput) -> Result<...> {
    let req = ChatRequest {
        model: ModelHint::Tier(ModelTier::Reasoning),
        system: Some(self.build_system_prompt()),     // see §11
        messages: self.build_messages(&input),
        structured_output: Some(self.output_schema()),
        ...
    };
    
    let req_ctx = RequestContext {
        trace_id: ctx.trace_id(),
        tenant_id: ctx.tenant_id(),
        session_id: ctx.session_id(),
        principal: ctx.principal.clone(),
        deadline: ctx.deadline,
        cancel: ctx.cancel.clone(),
        budget: ctx.task_budget.handle(),
        attributes: HashMap::new(),
    };
    
    let mut stream = ctx.llm_service.clone().call(req, req_ctx).await?;
    let mut accumulator = AgentMessageAccumulator::new();
    while let Some(event) = stream.next().await {
        accumulator.apply(event?);
    }
    
    let message: AgentMessage = accumulator.finalize()?;
    Ok(AgentStepResult { 
        output: message, 
        side_effects: vec![],
        usage: accumulator.usage(),
        raw_llm_response: Some(accumulator.raw_response_ref()),
    })
}
```

Runtime doesn't know how many Middleware layers live inside the Pipeline — to it, it's just an LlmService.

---

## 11. Prompt Builder Contract (coordinated with Cache)

Doc 03 §10.5 already specifies: the upper layer (i.e., Agent Runtime) must assemble in a three-segment Static Prefix / Project Anchor / Dynamic Suffix shape, so Cache can be shared across sessions.

Runtime provides a `PromptBuilder` abstraction:

```rust
pub trait PromptBuilder: Send + Sync {
    /// Fully static portion (system + tools schema + general conventions)
    /// Changes monthly; on change, the entire company cache misses
    fn static_prefix(&self) -> &str;
    
    /// Project-anchored portion (codebase / docs bound to a commit hash)
    /// Changes with commits; on change, that project's cache misses
    fn project_anchor(&self, project: &ProjectRef) -> Result<String, PromptError>;
    
    /// Dynamic suffix (content unique to this request)
    /// Changes every time; never enters cache
    fn dynamic_suffix(&self, input: &AgentInput) -> String;
    
    /// Assemble into a ChatRequest
    fn build(&self, project: &ProjectRef, input: &AgentInput) 
        -> Result<ChatRequest, PromptError>;
}
```

**Mandatory constraints**:
1. `static_prefix()` is byte-stable — order, whitespace, Markdown formatting must not be adjusted before a config change
2. `project_anchor()` must be a pure function of project state — same commit must return the same byte string
3. `dynamic_suffix()` must not influence the cache key — its position in the request is fixed at the end of messages

On startup, Runtime validates PromptBuilder implementation stability (multiple calls with the same input produce byte-identical output); refuses to start on inconsistency.

---

## 12. Frontend Contract: TrajectoryEvent stream

Runtime exposes only an event stream externally; it is unaware of the consumer:

```rust
#[async_trait]
pub trait Runtime: Send + Sync {
    /// Submit a new task
    async fn submit(
        &self,
        spec: TaskSpec,
        principal: Principal,
    ) -> Result<TaskHandle, RuntimeError>;
    
    /// Subscribe to a task's event stream (for Frontend Adapter)
    fn subscribe(
        &self,
        task: TaskId,
    ) -> BoxStream<'static, TrajectoryEvent>;
    
    /// Explicit control
    async fn cancel(&self, task: TaskId) -> Result<(), RuntimeError>;
    async fn suspend(&self, task: TaskId) -> Result<(), RuntimeError>;
    async fn resume(&self, task: TaskId, trigger: ResumeTrigger) -> Result<(), RuntimeError>;
    
    /// Historical query
    async fn query(&self, filter: TaskFilter) -> Result<Vec<TaskSnapshot>, RuntimeError>;
}

/// Events seen by the Frontend (subset of AgentEvent + derived events)
pub enum TrajectoryEvent {
    TaskStarted { task: TaskId, spec: TaskSpec },
    AgentInvoked { agent: AgentId, input_summary: String },
    AgentCompleted { agent: AgentId, output_summary: String, usage: Usage },
    AgentFailed { agent: AgentId, error: String, will_retry: bool },
    PartialArtifact { artifact: ArtifactPreview },        // streamable intermediate artifact
    TrajectoryForked { from: TrajectoryId, into: TrajectoryId, reason: String },
    BackingOff { reason: String, eta: Duration },
    Suspended { reason: SuspendReason },
    Completed { final_artifact: Artifact, total_usage: Usage, total_cost: f64 },
    Failed { reason: String, partial_result: Option<Artifact> },
}
```

**TrajectoryEvent is not a one-to-one exposure of AgentEvent** — the latter is the fine-grained internal event-sourcing record; the former is the "business event" that external consumers care about. An `EventProjector` performs the conversion in between:

```rust
trait EventProjector {
    fn project(&self, agent_event: &AgentEvent) -> Option<TrajectoryEvent>;
}
```

This separation lets the Runtime evolve its internal schema without breaking Frontend Adapters.

---

## 13. Testing Strategy

### 13.1 Replay-based testing

Record real-task event logs as fixtures, assert behavior on replay:

```rust
#[tokio::test]
async fn replays_known_task_deterministically() {
    let fixture = load_fixture("code_review_with_critic_reject.json");
    let runtime = test_runtime_with_recorded_llm_responses(&fixture).await;
    
    let task = runtime.submit(fixture.spec.clone(), test_principal()).await.unwrap();
    let events: Vec<_> = runtime.subscribe(task).collect().await;
    
    assert_event_sequence_matches(&events, &fixture.expected_events);
}
```

### 13.2 Crash recovery test

```rust
#[tokio::test]
async fn recovers_from_crash_mid_step() {
    let runtime = test_runtime();
    let task = runtime.submit(spec(), principal()).await.unwrap();
    
    // Simulate crash: abort runtime before step 3 completes
    let abort = tokio::spawn(async {
        wait_for_step(&runtime, task, 3, StepPhase::Started).await;
        runtime.simulate_crash().await;
    });
    
    abort.await.unwrap();
    
    // Restart runtime, verify recovery from step 3
    let runtime2 = test_runtime_resume_from_db().await;
    let final_events: Vec<_> = runtime2.subscribe(task).collect().await;
    
    assert_eq!(final_events.last().unwrap().step_seq(), expected_final_step);
}
```

### 13.3 Compensation test

```rust
#[tokio::test]
async fn backtrack_runs_compensations_in_reverse() {
    let comp_log = Arc::new(Mutex::new(Vec::new()));
    let runtime = test_runtime_with_compensation_recorder(comp_log.clone());
    
    let task = runtime.submit(spec_that_creates_3_resources_then_fails(), principal())
        .await.unwrap();
    runtime.wait_until_done(task).await;
    
    // Verify compensations ran in order R3, R2, R1
    assert_eq!(*comp_log.lock().unwrap(), vec!["R3", "R2", "R1"]);
}
```

### 13.4 Hard budget ceiling test

```rust
#[tokio::test]
async fn aborts_when_budget_exhausted() {
    let runtime = test_runtime();
    let task = runtime.submit(
        TaskSpec {
            budget: TaskBudget {
                max_tokens_total: 100,  // tiny, ensures the first LLM call overshoots
                ..Default::default()
            },
            ..spec()
        },
        principal(),
    ).await.unwrap();
    
    let result = runtime.wait_until_done(task).await;
    assert!(matches!(result, TaskResult::Failed { reason, .. } 
        if reason.contains("budget")));
}
```

---

## 14. Anti-pattern Checklist

1. **Don't use the LLM to decide "is the task done"** — use deterministic schema validation + Critic score thresholds. LLM self-evaluation has optimistic bias.
2. **Don't let Workers call each other directly** — all collaboration goes through the Context Store. Observability and replay depend on this invariant.
3. **Don't stuff "you may call the following tools, please choose" into the prompt** — Orchestrator decides in code; don't make the LLM the router.
4. **Don't ignore OTel trace propagation** — one trace_id per task, one span per agent hop. Production hallucination debug relies entirely on this.
5. **Don't let Critic and Worker share conversational context** — Critic must look at the output cold.
6. **Don't perform irreversible actions in intermediate steps** — defer all git push / send email / payment to the Commit Phase.
7. **Don't make idempotency_key an optional field** — enforce from Day 1; retrofitting is nearly impossible.
8. **Don't store large payloads directly in the event log** — go through ContentStore + ContentRef; events stay < 4KB.
9. **Don't assume LLM output non-determinism is "probabilistically acceptable"** — must be forced deterministic via cache or event capture, otherwise replay is unreliable.
10. **Don't let backtrack depth be unbounded** — hard ceiling (max_backtrack_depth); above that, suspend + notify a human.
11. **Don't silently continue when compensation fails** — escalate to human + freeze resources.
12. **Don't let Frontend Adapters read AgentEvent directly** — expose TrajectoryEvent via EventProjector to preserve internal schema-evolution freedom.
13. **Don't introduce timestamps / random numbers in PromptBuilder** — any dynamic variables must be moved to dynamic_suffix.
14. **Don't let Agents freely modify the Context Store** — they may only append events they themselves produced; they cannot modify other Agents' events.
15. **Don't stuff all context into one shot for long tasks** — control the window size handed to the next Agent via the Context Compactor.

---

## 15. Upstream/Downstream Contract

### Upstream (Frontend Adapter) commitments

- Submit a complete TaskSpec (including budget, principal, deadline) via `Runtime::submit`
- Consume TrajectoryEvent stream via `subscribe`
- Do not access AgentEvent / Trajectory / internal data structures directly
- Handle UI state for Suspend / Cancel signals

### Downstream (Pipeline) contract

- Each Agent gets Middleware-processed LLM capability via `AgentContext::llm_service`
- Consume LLM responses through the ChatEvent stream, aggregated by AgentMessageAccumulator
- Don't bypass the Pipeline to call Provider directly

### Side trait contract (Tool / MCP - Doc 05)

- `ToolRegistry` provides the tool list and invocation entry
- Tool calls must accept idempotency_key
- Tools must declare SideEffectKind; Runtime decides whether intermediate-step execution is allowed based on it

---

## 16. TODOs and Open Questions

- [ ] Practical depth ceiling for the Trajectory tree (Postgres index performance vs. readability)
- [ ] Specific compaction algorithm choice for Context Compactor (schema-aware vs. LLM summarization vs. hybrid)
- [ ] Whether direct adoption of Temporal/Restate is worth the comparison (heavy vs. light)
- [ ] Version-evolution strategy for the AgentMessage enum (serde compatibility vs. explicit schema migration)
- [ ] Cross-task Agent reuse (if two tasks both need the same agent, share an instance or create on demand)
- [ ] Wall-clock simulation during replay (should the event's created_at be honored on replay, or use current time)
- [ ] Visualization of large-scale Trajectory trees (with 10+ branches, how do TUI / Web present it)
- [ ] **Best-of-N / Parallel Sampling**: advanced mode — let the Orchestrator emit N plans and run them in parallel, returning on the first success. Suits code repair (N patches racing tests) / high-stakes decisions (N critics voting). Mirrors ChatGPT o1's internal mechanism. Cost is N×, but success rate is significantly higher; may become the topic of Doc 16 Advanced Patterns.
