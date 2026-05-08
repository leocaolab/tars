# Doc 05 — Tool / MCP / Skill Integration Design

> Scope: define the unified abstraction for external capabilities the Agent can invoke — local tools, MCP servers, composite Skills — along with permission model, invocation lifecycle, and side-effect tracking.
>
> Upstream: invoked by Doc 04 Agent Runtime (`AgentContext::tool_registry`).
>
> Downstream: may invoke Doc 01 LlmProvider (if a Skill includes LLM steps), external processes (MCP / CLI), HTTP APIs.

---

## 1. Relationship between the three concepts

Many frameworks conflate Tool / MCP / Skill, which entangles permission model, lifecycle, and invocation semantics. This doc strictly distinguishes three layers of abstraction:

```
                    ┌──────────────────────┐
                    │   Skill              │   high-level capability, may include multi-step LLM reasoning
                    │ (Composed Capability)│   e.g. "security-review the entire PR"
                    └──────────┬───────────┘
                               │ may orchestrate internally
                               ▼
                    ┌──────────────────────┐
                    │   Tool               │   single atomic call with explicit input/output schema
                    │ (Atomic Function)    │   e.g. "read file" / "execute SQL"
                    └──────────┬───────────┘
                               │ implementation may come from
                ┌──────────────┼──────────────┐
                ▼              ▼              ▼
         ┌──────────┐   ┌────────────┐  ┌──────────┐
         │ Built-in │   │ MCP Server │  │ HTTP API │
         │ (Rust fn)│   │ (subproc)  │  │ (REST)   │
         └──────────┘   └────────────┘  └──────────┘
```

| Concept | Abstraction level | Caller | Visibility | Example |
|---|---|---|---|---|
| **Tool** | atomic function | Agent (Worker / Orchestrator) | LLM references it by name in tool_use | `read_file(path)`, `query_postgres(sql)` |
| **MCP Server** | source/container of Tools | internal to ToolRegistry | Tools are discovered and invoked via the MCP protocol | `mcp-filesystem`, `mcp-github` |
| **Skill** | composite capability | Agent (typically Orchestrator), referenced by PlanIssued | LLM does not see Skill internals; it sees a higher-level capability | "review this PR" (internally read_file + AST parsing + LLM reasoning + writing comments) |

**Core distinctions**:
- A Tool is **atomic** — one call, one result, no LLM involvement (unless the Tool implementation happens to wrap an LLM)
- A Skill is **composite** — may orchestrate multiple Tool calls + LLM reasoning + intermediate state
- MCP is a **protocol**, not a concept — it defines "how remote Tools are discovered and invoked", and is one transport mechanism for Tools

---

## 2. Design goals

| Goal | Description |
|---|---|
| **Three layers stay clean** | Tool / Skill / MCP each have independent traits, lifecycles, and permission models, and don't pollute each other |
| **Strict Tool schema** | Input/output uses JSON Schema strict mode, consistent with provider-side structured output in Doc 01 §9 |
| **Idempotency required** | Every Tool invocation accepts an idempotency_key, satisfying the replay contract in Doc 04 §7 |
| **Side-effect classification mandatory** | Each Tool declares a SideEffectKind (Doc 04 §4.4); Runtime decides whether it can run during intermediate steps based on this |
| **IAM gating** | Every Tool has minimum scope requirements; if the Principal does not satisfy them, the call is rejected up front |
| **MCP subprocess reuse** | Long-lived MCP servers are shared across requests, mirroring Doc 01 §6.2 CLI mode |
| **Skill portability** | Skill definition is decoupled from concrete Tool implementations; the same Skill behaves consistently across different Tool implementations |
| **Streaming output** | Long-running Tools (grep over a large codebase / running a test suite) support stream output, not all-or-nothing return |

**Anti-goals**:
- Don't let the LLM freely choose Tools / Skills — the callable set is bounded by the Orchestrator in code
- Don't do prompt assembly at the Tool layer — if a Skill calls an LLM internally, it goes through the Doc 02 Pipeline
- Don't allow MCP servers to receive external traffic directly — all MCP calls go through this process's ToolRegistry
- Don't expose a Tool's internal idempotency state to the LLM — the LLM sees a clean "call + result"

---

## 3. Tool core abstraction

### 3.1 Tool trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn id(&self) -> &ToolId;
    fn descriptor(&self) -> &ToolDescriptor;
    
    /// Atomic invocation
    async fn invoke(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError>;
    
    /// Streaming invocation (default impl: wrap one-shot return into a single-element stream)
    async fn invoke_stream(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<BoxStream<'static, Result<ToolEvent, ToolError>>, ToolError> {
        let result = self.invoke(args, ctx).await?;
        Ok(Box::pin(futures::stream::once(async move { 
            Ok(ToolEvent::Complete(result)) 
        })))
    }
}

pub struct ToolDescriptor {
    pub id: ToolId,
    pub display_name: String,
    pub description: String,                 // this is the tool description shown to the LLM
    pub input_schema: JsonSchema,
    pub output_schema: JsonSchema,
    pub side_effect: SideEffectKind,         // Doc 04 §4.4
    pub required_scopes: Vec<Scope>,         // IAM minimum scopes
    pub idempotent: bool,                    // naturally idempotent (read-class typically yes)
    pub typical_latency: Duration,           // for budget estimation and timeout setting
    pub timeout: Duration,                   // hard upper bound
    pub source: ToolSource,                  // Built-in / Mcp / Http / Subprocess
    pub version: SemanticVersion,
}

pub struct ToolContext {
    pub trajectory: TrajectoryId,
    pub step_seq: u32,
    pub idempotency_key: String,             // required per Doc 04 §7
    pub principal: Principal,
    pub deadline: Instant,
    pub cancel: CancellationToken,
    pub budget: BudgetView,                  // tool calls also count toward budget (duration / call count)
}

pub enum ToolEvent {
    Started { tool: ToolId, args_summary: String },
    Progress { message: String, percent: Option<f32> },
    PartialOutput { chunk: serde_json::Value },
    Complete(ToolOutput),
}

pub struct ToolOutput {
    pub result: serde_json::Value,           // must conform to descriptor.output_schema
    pub side_effects: Vec<SideEffect>,       // actual side effects produced
    pub usage: ToolUsage,                    // resource consumption (CPU time / network / tokens)
}
```

### 3.2 ToolError taxonomy

```rust
pub enum ToolError {
    InvalidArguments { schema_violation: String },
    PermissionDenied { required: Scope, principal: Principal },
    Timeout { elapsed: Duration, limit: Duration },
    Cancelled,
    
    /// Resource missing / state error and similar business-level errors; the LLM can learn from them
    BusinessError { code: String, message: String, retriable: bool },
    
    /// Tool implementation internal error (MCP server crash / network jitter)
    Infrastructure { source: Box<dyn StdError + Send + Sync> },
    
    /// Side effect succeeded but response was lost (network dropped before ack)
    /// idempotency_key makes retry safe
    AmbiguousOutcome { idempotency_key: String, message: String },
}

impl ToolError {
    pub fn class(&self) -> ToolErrorClass {
        use ToolError::*;
        match self {
            InvalidArguments { .. } | PermissionDenied { .. } => ToolErrorClass::Permanent,
            BusinessError { retriable: false, .. } => ToolErrorClass::Permanent,
            Timeout { .. } | Cancelled | Infrastructure { .. } 
                | AmbiguousOutcome { .. } => ToolErrorClass::Retriable,
            BusinessError { retriable: true, .. } => ToolErrorClass::Retriable,
        }
    }
}
```

**Important**: the `retriable` field on BusinessError must be set explicitly by the Tool implementation. Agent / LLM should not guess — "file not found" is permanent, "temporarily can't reach the database" is retriable.

### 3.3 Input/output Schema and LLM coordination

The tool signature seen by the LLM comes directly from `ToolDescriptor.input_schema`:

```rust
// Provider adapter layer (Doc 01 §8) translates to the corresponding provider's tool spec
fn descriptor_to_openai_tool(desc: &ToolDescriptor) -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": desc.id.as_str(),
            "description": desc.description,
            "parameters": desc.input_schema,
            "strict": true,
        }
    })
}
```

**Mandatory rules**:
1. input_schema must be acceptable simultaneously to OpenAI strict mode / Anthropic tool use / Gemini functionCalling. This means a subset syntax (no `oneOf` / `not` / complex `pattern`).
2. output_schema is not just documentation — Runtime validates Tool returns against it, and reports `BusinessError { code: "schema_violation", retriable: false }` on violation.
3. The description must explain **when to use it**, not just **what it does**. The LLM picks Tools mainly based on description.

---

## 4. ToolRegistry

### 4.1 Core trait

```rust
#[async_trait]
pub trait ToolRegistry: Send + Sync {
    /// List all Tools visible to a given Agent role under a given Principal
    async fn list_for(
        &self, 
        agent: &AgentId, 
        principal: &Principal,
    ) -> Vec<Arc<dyn Tool>>;
    
    /// Look up by ID
    async fn get(&self, tool_id: &ToolId) -> Option<Arc<dyn Tool>>;
    
    /// Invoke (with IAM / timeout / audit)
    async fn invoke(
        &self,
        tool_id: &ToolId,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError>;
    
    /// Streaming invocation
    async fn invoke_stream(
        &self,
        tool_id: &ToolId,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<BoxStream<'static, Result<ToolEvent, ToolError>>, ToolError>;
}
```

### 4.2 Tool invocation pipeline

Every Tool invocation goes through its own mini middleware stack (lighter than the LLM Pipeline but isomorphic):

```
Tool Invocation Request
        │
        ▼
┌─────────────────┐
│ IAM Gate        │  ← required_scopes ⊆ principal.scopes ?
└────────┬────────┘
         ▼
┌─────────────────┐
│ Idempotency     │  ← same key already has a completed call → return cached result directly
│ Cache           │  
└────────┬────────┘
         ▼
┌─────────────────┐
│ Side Effect     │  ← intermediate steps disallow Irreversible
│ Gate            │
└────────┬────────┘
         ▼
┌─────────────────┐
│ Budget Check    │  ← tool call count / estimated duration
└────────┬────────┘
         ▼
┌─────────────────┐
│ Audit Log       │  ← write to event log (replayable)
└────────┬────────┘
         ▼
┌─────────────────┐
│ Timeout Wrap    │  ← descriptor.timeout
└────────┬────────┘
         ▼
   Tool::invoke
```

Failures at every layer are explicit `ToolError` — no silent error swallowing.

### 4.3 Idempotency Cache

Tool call results are cached by `idempotency_key` (a different store from the Doc 03 LLM cache — don't confuse them):

```rust
pub struct IdempotencyCache {
    /// idempotency_key -> ToolOutput
    /// TTL typically 24h, matching the maximum trajectory lifetime
    backend: Arc<dyn KVStore>,
}

impl IdempotencyCache {
    pub async fn get_or_compute<F, Fut>(
        &self,
        key: &str,
        compute: F,
    ) -> Result<ToolOutput, ToolError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<ToolOutput, ToolError>>,
    {
        if let Some(cached) = self.backend.get(key).await? {
            return Ok(cached);
        }
        let result = compute().await?;
        self.backend.put(key, &result, Duration::from_hours(24)).await?;
        Ok(result)
    }
}
```

**Key boundaries**:
- Only **successful** outputs are cached. Failures aren't cached, otherwise retries would always get the same error
- The "idempotent" semantics of side-effecting Tools are guaranteed by the **Tool implementation** in cooperation with idempotency_key (using the key as an external dedup ID), not simulated at the cache layer

### 4.4 IAM Gate

Every Tool has `required_scopes`, validated on invocation as `principal.scopes ⊇ required_scopes`:

```rust
async fn check_iam(tool: &dyn Tool, principal: &Principal) -> Result<(), ToolError> {
    let required = &tool.descriptor().required_scopes;
    for scope in required {
        if !principal.has_scope(scope) {
            return Err(ToolError::PermissionDenied { 
                required: scope.clone(), 
                principal: principal.clone() 
            });
        }
    }
    Ok(())
}
```

**Difference from Doc 02 §4.2 IAM Middleware**:
- LLM Pipeline IAM checks "can the caller talk to this model"
- Tool IAM checks "can the caller invoke this Tool"
- The two layers are independent — a Principal may be allowed to call the LLM but not `delete_database`

---

## 5. MCP integration

### 5.1 MCP protocol summary

Model Context Protocol (proposed by Anthropic) defines three resource types:
- **Tools**: invokable functions (same semantics as Tool in §3 of this doc)
- **Resources**: readable data (URI-addressed)
- **Prompts**: reusable prompt templates

Transports:
- **stdio**: subprocess + JSON-RPC over stdin/stdout
- **SSE**: HTTP server-sent events
- **HTTP**: pure REST

Our Runtime mainly consumes **Tools**, with limited support for Resources and Prompts (see §5.4).

### 5.2 MCP Server adaptation

An MCP server is one source of Tools, adapted to the `Tool` trait by `McpToolProvider`:

```rust
pub struct McpToolProvider {
    server_id: McpServerId,
    transport: McpTransport,
    discovered_tools: Arc<RwLock<HashMap<String, McpToolHandle>>>,
}

pub enum McpTransport {
    Stdio { 
        binary: PathBuf, 
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Sse { url: Url, auth: Auth },
    Http { url: Url, auth: Auth },
}

impl McpToolProvider {
    /// At startup, call MCP list_tools and register the server-provided tools to ToolRegistry
    pub async fn discover(&self) -> Result<Vec<ToolDescriptor>, McpError>;
    
    /// Wrap each MCP tool as a Rust Tool implementation
    pub fn wrap_as_tool(&self, mcp_handle: McpToolHandle) -> Arc<dyn Tool> {
        Arc::new(McpToolAdapter { 
            provider: self.clone(),
            handle: mcp_handle,
        })
    }
}

struct McpToolAdapter {
    provider: McpToolProvider,
    handle: McpToolHandle,
}

#[async_trait]
impl Tool for McpToolAdapter {
    async fn invoke(&self, args: serde_json::Value, ctx: ToolContext) 
        -> Result<ToolOutput, ToolError> 
    {
        // 1. Send call_tool request via the provider's transport
        let response = self.provider.call_tool(&self.handle.name, args, &ctx).await?;
        
        // 2. Translate MCP response to ToolOutput, validate output schema
        Ok(ToolOutput {
            result: response.content,
            side_effects: response.declared_side_effects,
            usage: ToolUsage::from_mcp(&response.usage),
        })
    }
}
```

### 5.3 Subprocess management for stdio MCP servers

Stdio MCP server lifecycle management is fully isomorphic to Doc 01 §6.2 Claude CLI — long-lived, reused per session, killed on idle timeout, kill_on_drop:

```rust
pub struct StdioMcpProcess {
    config: McpStdioConfig,
    sessions: Arc<DashMap<SessionId, Arc<McpSession>>>,
}

struct McpSession {
    child: Mutex<tokio::process::Child>,           // .kill_on_drop(true)
    stdin_tx: mpsc::Sender<JsonRpcRequest>,
    response_dispatcher: broadcast::Sender<JsonRpcResponse>,
    last_used: AtomicInstant,
    janitor: JoinHandle<()>,
}
```

**Key decisions** (consistent with Doc 01 §6.2):
- **One subprocess per SessionId** — MCP state is shared within a session, isolated across sessions
- **Kill on 5-minute idle** — avoid leaks
- **Multi-tenant must use independent HOME** — MCP server auth state (e.g. OAuth tokens) is tenant-isolated
- **kill_on_drop(true)** as a safety net
- **Cancel safety**: when an in-flight tool call is dropped, send a JSON-RPC `cancelled` notification to the server

### 5.4 Limited support for Resources and Prompts

| MCP concept | Runtime support | Rationale |
|---|---|---|
| Tools | Full | core use case |
| Resources | Skill-internal use only | LLM does not access URIs directly; a Skill implementation may read a resource and inject it into the prompt |
| Prompts | Not integrated | Prompt assembly is the responsibility of Doc 04 §11 PromptBuilder; we don't let MCP servers decide prompts |

**Why we don't let MCP provide prompts**: an MCP server is **partially trusted external code** — letting it define system prompts hands over control of "how our Agent thinks". Prompts must be assembled in our own code.

### 5.5 MCP server security boundary

An MCP server is **partially trusted code** — it may be community-maintained, third-party, or user-installed. Treatment:

1. **MCP servers must not execute in the main process address space** — they must be subprocesses or remote
2. **Stdio MCP servers must have an allowed-binary whitelist** — arbitrary path execution is disallowed
3. **HTTP/SSE MCP server URLs must be explicitly declared in config** — no dynamic URL discovery
4. **MCP-server-declared tools still require explicit enablement in our ToolRegistry** — no auto-register
5. **MCP tool required_scopes are overridden by config** — scopes declared by the MCP server itself are only hints; final permissions are determined by local config

```toml
[mcp_servers.filesystem]
type = "stdio"
binary = "/usr/local/bin/mcp-filesystem"
args = ["--root", "/srv/projects"]
auth = { kind = "delegate", per_tenant_home = true }

[mcp_servers.filesystem.tools.read_file]
enabled = true
required_scopes = ["fs:read"]                # overrides server's own declaration
override_timeout_secs = 30

[mcp_servers.filesystem.tools.delete_file]
enabled = false                               # fully disabled, even though the server provides it
```

---

## 6. Skill abstraction

### 6.1 Skill trait

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    fn id(&self) -> &SkillId;
    fn descriptor(&self) -> &SkillDescriptor;
    
    /// Execute the Skill — internally may be multi-step (tool call + LLM + state machine)
    /// Returns an event stream, similar to Tool::invoke_stream but with richer semantics
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: SkillContext,
    ) -> Result<BoxStream<'static, Result<SkillEvent, SkillError>>, SkillError>;
}

pub struct SkillDescriptor {
    pub id: SkillId,
    pub display_name: String,
    pub description: String,
    pub input_schema: JsonSchema,
    pub output_schema: JsonSchema,
    pub required_scopes: Vec<Scope>,
    pub required_tools: Vec<ToolId>,         // which Tools must be available to run
    pub estimated_cost: CostEstimate,
    pub max_duration: Duration,
    pub source: SkillSource,
}

pub enum SkillSource {
    /// Pure Rust implementation, deterministic logic
    Native,
    /// Defined by prompt template + tool orchestration, LLM-driven
    Declarative { spec_file: PathBuf },
    /// Effectively a sub-Agent
    SubAgent { agent_id: AgentId },
}

pub struct SkillContext {
    pub trajectory: TrajectoryId,            // parent trajectory
    pub principal: Principal,
    pub deadline: Instant,
    pub cancel: CancellationToken,
    pub budget: BudgetView,
    
    /// A Skill can call Tools and the LLM
    pub tool_registry: Arc<dyn ToolRegistry>,
    pub llm_service: Arc<dyn LlmService>,
}

pub enum SkillEvent {
    Started { skill: SkillId },
    SubStepStarted { step_name: String },
    ToolCalled { tool: ToolId, args_summary: String },
    LlmInvoked { model_tier: ModelTier },
    PartialResult { artifact: ArtifactPreview },
    SubStepCompleted { step_name: String },
    Completed { final_output: serde_json::Value, usage: SkillUsage },
}
```

### 6.2 Skill vs Agent / Tool

Skill sits abstractly between Agent and Tool:

| Dimension | Tool | Skill | Agent |
|---|---|---|---|
| Caller | Agent (Worker / Orchestrator) | Agent (typically Orchestrator) | Runtime |
| Internal state | none | possibly (multi-step) | yes (trajectory state) |
| Includes LLM | typically no (unless the Tool wraps an LLM service) | possibly | almost always |
| Side-effect tracking | single point (declared in descriptor) | accumulated (each internal Tool call accumulates) | accumulated (event log) |
| Failure semantics | single error | partial completion + error | whole trajectory abandoned + compensation |

### 6.3 Two implementation modes for Skills

**Native (recommended for critical paths)**:
```rust
pub struct CodeReviewSkill { /* ... */ }

#[async_trait]
impl Skill for CodeReviewSkill {
    async fn execute(&self, args: serde_json::Value, ctx: SkillContext) 
        -> Result<BoxStream<...>, SkillError> 
    {
        // 1. Parse args
        let CodeReviewArgs { repo, pr_number } = serde_json::from_value(args)?;
        
        // 2. Call Tool to fetch diff
        let diff = ctx.tool_registry.invoke(
            &"git.fetch_pr_diff".into(),
            json!({ "repo": repo, "pr": pr_number }),
            tool_ctx_from(&ctx, 1),
        ).await?;
        
        // 3. Parse AST (pure compute, not a Tool call)
        let ast = parse_diff_to_ast(&diff)?;
        
        // 4. Call LLM to review
        let review = ctx.llm_service.clone().call(
            ChatRequest { /* ... */ },
            req_ctx_from(&ctx),
        ).await?;
        
        // 5. Write comments (side effect — must be in commit phase)
        // ... 
    }
}
```

All logic of a native Skill lives in Rust code — readable, testable, refactorable. Suitable for stable, critical-path capabilities.

**Declarative (suitable for experimental / user-customizable)**:
```yaml
# skills/security_audit.yaml
id: security_audit
description: "comprehensive security review"
required_tools: [git.fetch_pr_diff, sast.run_semgrep]
steps:
  - name: fetch_diff
    tool: git.fetch_pr_diff
    args:
      repo: "${input.repo}"
      pr: "${input.pr}"
  - name: run_sast
    tool: sast.run_semgrep
    args:
      diff: "${steps.fetch_diff.output}"
  - name: synthesize
    llm:
      tier: reasoning
      prompt_template: security_synthesis.j2
      input:
        diff: "${steps.fetch_diff.output}"
        sast_findings: "${steps.run_sast.output}"
output: "${steps.synthesize.output}"
```

A declarative Skill is interpreted by a generic SkillExecutor. **Risk**: YAML expressiveness is limited; complex logic (conditional branches / loops / error handling) makes specs hard to maintain. Recommend it only for linear pipelines.

For complex, branching, or critic-loop logic — make it a SubAgent (see §6.4).

### 6.4 Skill as SubAgent

The most complex Skills are simply a SubAgent — with their own trajectory, event log, and critic loop, but exposed externally as a single Skill invocation:

```rust
pub struct SubAgentSkill {
    inner_runtime: Arc<dyn Runtime>,
    agent_blueprint: AgentBlueprint,
}

#[async_trait]
impl Skill for SubAgentSkill {
    async fn execute(&self, args: serde_json::Value, ctx: SkillContext) 
        -> Result<BoxStream<...>, SkillError> 
    {
        // Create a child task under the parent trajectory
        let sub_task = TaskSpec {
            parent_trajectory: Some(ctx.trajectory),
            blueprint: self.agent_blueprint.clone(),
            input: args,
            budget: ctx.budget.subdivide(0.3).unwrap_or_default(),  // child task gets 30% of the budget
            ...
        };
        
        let handle = self.inner_runtime.submit(sub_task, ctx.principal.clone()).await?;
        let inner_events = self.inner_runtime.subscribe(handle.task_id);
        
        // Project inner TrajectoryEvent to outer SkillEvent
        let mapped = inner_events.map(|ev| Self::project(ev));
        Ok(Box::pin(mapped))
    }
}
```

**Key**: the child task's trajectory is a child node of the parent trajectory (Doc 04 §3.1 `parent` field), and its event stream is absorbed into the parent trajectory's event log. Backtrack / resume semantics are fully inherited.

---

## 7. Tool invocation lifecycle (in coordination with Doc 04 side-effect classification)

Each Tool call produces a set of `SideEffect`s, classified per Doc 04 §4.4. Runtime processing before and after the call:

### 7.1 Before invocation

```rust
fn check_side_effect_allowed(
    tool: &dyn Tool,
    trajectory_phase: TrajectoryPhase,
) -> Result<(), ToolError> {
    let kind = tool.descriptor().side_effect.clone();
    
    match (kind, trajectory_phase) {
        // Pure / Isolated / Reversible allowed in any phase
        (SideEffectKind::PureCompute, _) => Ok(()),
        (SideEffectKind::IsolatedWrite { .. }, _) => Ok(()),
        (SideEffectKind::ReversibleResource { .. }, _) => Ok(()),
        
        // Irreversible only allowed in Commit Phase
        (SideEffectKind::Irreversible { .. }, TrajectoryPhase::Commit) => Ok(()),
        (SideEffectKind::Irreversible { resource }, _) => {
            Err(ToolError::PermissionDenied {
                required: Scope::CommitPhaseAccess,
                principal: ctx.principal.clone(),
            })
        }
    }
}
```

### 7.2 Register compensations after invocation

A successful ReversibleResource call must register a compensation action into the trajectory's pending_compensations:

```rust
async fn invoke(&self, ...) -> Result<ToolOutput, ToolError> {
    let result = self.actual_invoke(...).await?;
    
    for effect in &result.side_effects {
        if let SideEffectKind::ReversibleResource { cleanup, .. } = &effect.kind {
            self.trajectory_store.append_compensation(
                ctx.trajectory,
                CompensationAction {
                    id: CompensationId::new(),
                    action: cleanup.clone(),
                    target: effect.resource.clone(),
                    idempotency_key: effect.idempotency_key.clone(),
                },
            ).await?;
        }
    }
    
    Ok(result)
}
```

On backtrack (Doc 04 §6) these compensations execute in LIFO order.

---

## 8. Streaming Tool output

Some Tools are inherently long-running with progressive output:
- `grep_codebase` emits results as it searches
- `run_test_suite` runs tests one by one
- `download_dataset` progress bar
- `query_postgres` row stream for large queries

`Tool::invoke_stream` returns `BoxStream<Result<ToolEvent>>`, feeding the Frontend's real-time progress display.

**Constraint**: a streaming Tool must still aggregate to a single ToolOutput for the LLM as a tool result. The aggregation strategy is implemented by the Tool itself (concat text / merge JSON / take last).

---

## 9. Configuration shape

```toml
# Built-in Tool
[[tools.builtin]]
id = "git.fetch_pr_diff"
required_scopes = ["git:read"]
timeout_secs = 30

[[tools.builtin]]
id = "fs.read_file"
required_scopes = ["fs:read"]
timeout_secs = 5

# MCP Server
[mcp_servers.github]
type = "stdio"
binary = "/usr/local/bin/mcp-github"
args = []
mode = "long_lived"
session_idle_timeout_secs = 300
auth = { kind = "delegate", per_tenant_home = true }

[mcp_servers.github.tools.create_issue]
enabled = true
required_scopes = ["github:write"]
side_effect_override = "Irreversible"        # forced upgrade to irreversible, only allowed in commit phase

[mcp_servers.github.tools.read_repo]
enabled = true
required_scopes = ["github:read"]

# Skill
[[skills]]
id = "security_audit_full"
source = { type = "native", impl = "crate::skills::SecurityAuditSkill" }
required_scopes = ["security:read", "github:read"]
required_tools = ["git.fetch_pr_diff", "sast.run_semgrep"]
max_duration_secs = 600

[[skills]]
id = "code_smell_detection"
source = { type = "declarative", spec_file = "skills/code_smell.yaml" }

# Idempotency cache
[tool_invocation.idempotency]
backend = "redis"
ttl_secs = 86400

# Visible Tool set per Agent role
[agent_capabilities.orchestrator]
tools = ["fs.read_file", "git.fetch_pr_diff"]   # read-class only
skills = ["security_audit_full", "code_smell_detection"]

[agent_capabilities.code_review_worker]
tools = ["fs.read_file", "git.fetch_pr_diff", "sast.run_semgrep"]

[agent_capabilities.commit_phase_agent]
tools = ["github.create_issue", "git.push_branch"]   # only commit-phase agents may call irreversibles
```

---

## 10. Testing strategy

### 10.1 Tool unit tests

Every Tool tested independently, with external dependencies mocked:

```rust
#[tokio::test]
async fn read_file_returns_business_error_on_missing() {
    let tool = ReadFileTool::new(MockFs::without_files());
    let result = tool.invoke(json!({ "path": "/nonexistent" }), test_ctx()).await;
    
    assert!(matches!(
        result,
        Err(ToolError::BusinessError { code, retriable: false, .. })
            if code == "file_not_found"
    ));
}
```

### 10.2 Schema conformance

Every Tool implementation runs the same schema validation suite:
- input_schema accepted by OpenAI strict mode (no `oneOf` / complex pattern)
- fields described in output_schema all present in actual output
- input/output round-trip successfully with serde

### 10.3 MCP integration tests

Spin up a reference MCP server (filesystem reference impl) and verify:
- discovery lists all declared tools
- list_for filters by IAM
- invoke succeeds end-to-end
- subprocess is killed after idle timeout
- subprocess auto-restarts on next invoke after a crash

### 10.4 Idempotency tests

```rust
#[tokio::test]
async fn duplicate_invocation_returns_cached_output() {
    let tool = SpyTool::new();
    let registry = test_registry_with(&tool);
    
    let key = "test-key-1".to_string();
    let ctx1 = ToolContext { idempotency_key: key.clone(), ..test_ctx() };
    let ctx2 = ToolContext { idempotency_key: key, ..test_ctx() };
    
    let r1 = registry.invoke(&tool.id(), json!({}), ctx1).await.unwrap();
    let r2 = registry.invoke(&tool.id(), json!({}), ctx2).await.unwrap();
    
    assert_eq!(r1.result, r2.result);
    assert_eq!(tool.invocation_count(), 1);  // actually invoked only once
}
```

### 10.5 Side-effect gate tests

```rust
#[tokio::test]
async fn irreversible_tool_rejected_outside_commit_phase() {
    let registry = test_registry();
    let ctx = ToolContext { 
        trajectory_phase: TrajectoryPhase::Worker,
        ..test_ctx() 
    };
    
    let result = registry.invoke(
        &"github.create_issue".into(),
        json!({ "title": "x" }),
        ctx,
    ).await;
    
    assert!(matches!(result, Err(ToolError::PermissionDenied { .. })));
}
```

---

## 11. Anti-pattern checklist

1. **Don't let the LLM decide which Skill to invoke** — the Orchestrator bounds the callable set in code; the LLM only chooses Tools within that set.
2. **Don't directly trust the scopes declared by an MCP server** — local config overrides.
3. **Don't spawn MCP subprocesses outside ToolRegistry** — all subprocess lifecycles are managed centrally.
4. **Don't let Tools hold mutable state across calls** — externalize state (DB / cache).
5. **Don't read env vars inside Tool implementations** — inject configuration via ToolContext.
6. **Don't hard-code Tool name strings inside Skill implementations** — declare via `required_tools` and obtain by injection.
7. **Don't initiate LLM calls inside a Tool without going through the Pipeline** — if a Tool needs an LLM, it must go through ctx.llm_service.
8. **Don't let MCP control system behavior via Resources / Prompts** — Resources are Skill-internal only; Prompts are not integrated.
9. **Don't reuse the same idempotency_key across trajectories** — keys must include trajectory_id to avoid false hits.
10. **Don't have declarative skills express conditional branches / loops** — at that complexity, switch to SubAgent or native.
11. **Don't ignore cancel on Tool calls** — long-running tasks must listen on cancel.cancelled().
12. **Don't invoke an Irreversible Tool before the commit phase** — Runtime forces a reject, but the Skill design itself should avoid this.
13. **Don't display an MCP tool's description directly to the end user** — it may be untrusted content (prompt injection attack surface).
14. **Don't have Tool output_schema use `additionalProperties: true`** — every field must be explicitly declared, otherwise downstream Agents get unpredictable structures.
15. **Don't reuse the LLM cache to cache Tool output** — the two stores are independent with different semantics (LLM cache keys on prompt hash, Tool cache keys on idempotency_key).

---

## 12. Contracts with upstream/downstream

### Upstream (Agent / Skill) commitments

- Obtain ToolRegistry via `AgentContext::tool_registry`; do not bypass
- Provide a complete ToolContext on every call (with idempotency_key, trajectory, principal)
- Don't assume Tool call ordering — concurrent tool calls must handle interleaving correctly

### Downstream (MCP Server / HTTP API / Built-in) contract

- Must correctly process inputs that conform to input_schema
- Output must conform to output_schema (Runtime validates)
- Must accept and respect idempotency_key (a duplicate key must return the same result or a Conflict error)
- Long tasks support cancel signals
- Side-effect list matches declared_side_effects

### Runtime / Pipeline boundary

- Tool call events (ToolCalled / ToolCompleted / ToolFailed) are written to the Doc 04 §3.2 event log
- Tool failure error class (Permanent / Retriable) influences the Doc 04 §6 backtrack decision
- Tool IAM checks **and** the Doc 02 §4.2 LLM Pipeline IAM checks are **two independent layers** — passing LLM IAM does not imply passing Tool IAM

---

## 13. TODOs and open questions

- [ ] MCP protocol version evolution strategy (how to maintain compatibility with old servers when v1 → v2)
- [ ] Skill version management (can different versions of the same Skill coexist, and how is routing done)
- [ ] Declarative Skill executor selection (custom mini DSL vs reusing Step Functions / Argo Workflows)
- [ ] Cross-process / cross-node Tool call tracing (propagate OTel context into MCP)
- [ ] MCP server resource quotas (CPU / memory / file handles) — cgroups vs nsjail vs trust
- [ ] Tool cost-billing dimensions (per call / per duration / per data volume)
- [ ] Whether intermediate artifacts produced inside a Skill should be persisted to ContentStore (debug value vs storage cost)
- [ ] MCP server health check protocol (heartbeat / circuit breaker)
- [ ] **RAG / Vector Store integration section**: the doc currently states no built-in vector store (positioned as application-layer), but should add a section on "hooks for Tool/Skill integration with vector retrieval". Personal mode recommends in-process mmap solutions (faiss-rs / lance) to avoid spinning up a standalone vector DB process; Team/SaaS mode recommends Qdrant/Milvus as an external service tool.
