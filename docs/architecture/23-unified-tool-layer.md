# Doc 23 — Unified tool layer (design)

> Status: design, 2026-06-10. Implements the tool-layer half of
> [Doc 22](./22-codex-tui-port.md) (T1–T3) and pays down the
> two-`ToolRegistry` fork + `skills()→tools` gap from
> [Doc 21 §2/§5](./21-tars-agent-impl-notes.md). Reuses the tool contract
> from [Doc 05](./05-tools-mcp-skills.md) and the permission model from
> [Doc 20](./20-agent-abstraction.md).
>
> *(Generated via the `design` skill — grounded in the codebase; every
> reuse is cited `file:line`.)*

## 1. Overview & goal

TARS has two incompatible tool systems — `tars_tools::Tool`
(`execute(args, ToolContext{cwd,cancel}) → ToolResult`,
`tool.rs:144`) and a second `tars_runtime::session::Tool`
(`call(args) → JsonValue`, no context, `session.rs:133`). Coding tools
(write/edit/bash) live only on the first; `Session` (the interactive
backend the Codex-TUI port needs) drives only the second. Permissions are
enforced in **one** place (`worker.rs:389`), so `Session`-dispatched tools
run **ungated**. There is no sandbox, no human-approval channel (`Ask` ==
`Deny`), and no `skill→tool` binding (Doc 21 §5).

**Goal:** one `Tool` contract, one registry, dispatched identically by
`Session` and `Worker`, with the permission gate *inside* dispatch, an
`ApprovalSink` seam that makes `Ask` real, and `ToolContext` extended with
the `sandbox`/`approval` fields that Doc 22's sandbox/exec/apply-patch lifts
plug into.

**Non-goals:** the actual Seatbelt/Landlock sandbox lift (Doc 22 T2, its own
design); MCP client (Doc 22 M5); the TUI itself (Doc 22). This doc delivers
the *layer they attach to*.

## 2. Critical User Journeys (CUJs)

- **CUJ-1 — Session runs a gated tool.** An interactive `Session` (Codex-TUI
  backend) over a model that emits a `write_file` call: the call is
  permission-checked, executed against the session's `cwd`, and the result
  fed back into the turn — same code path a `Worker` uses.
- **CUJ-2 — Human approves a risky action.** A tool whose skill is `Ask`
  (e.g. `bash`) pauses; the runtime surfaces an approval prompt; on **allow**
  it runs, on **deny** the model gets a clean refusal `ToolResult` and
  continues. Headless callers fail closed (`Ask` → deny).
- **CUJ-3 — One tool, both drivers.** A single `Arc<dyn Tool>` registered
  once is callable from both `Session::send` and `WorkerAgent::run` with
  identical dispatch, permission, and `cwd` semantics. No second registration.
- **CUJ-4 — Sandbox seam is honored.** A tool reads `ctx.sandbox` and a
  future sandboxed `exec` confines its subprocess to the policy's writable
  roots; tools that ignore it behave exactly as today (additive).
- **CUJ-5 — Skill resolves to tools.** An agent advertising skill `fs.edit`
  has exactly the concrete tools that back it registered, with no drift
  between advertised `SkillSet` and dispatchable `ToolRegistry`.

## 3. Feature list

| Feature | Serves | Notes |
|---|---|---|
| F1 — Single `Tool` trait + `ToolContext` (one crate) | CUJ-1,3 | Keep `tars_tools::Tool`; retire `session.rs`'s |
| F2 — Permission gate inside registry dispatch | CUJ-1,2 | Move the `worker.rs:389` check down so all callers inherit it |
| F3 — `ApprovalSink` seam (`Ask` → human) | CUJ-2 | New trait on `ToolContext`; TUI + headless impls |
| F4 — `Session` rewired onto `tars_tools::ToolRegistry` | CUJ-1,3 | Delete the in-`session.rs` registry |
| F5 — `ToolContext.sandbox` field + policy type | CUJ-4 | Seam only; consumers later (Doc 22 T2) |
| F6 — `SkillSet ↔ ToolRegistry` binding | CUJ-5 | Skill name → tool(s); construct-time consistency check |

## 4. Requirements

**Functional**

| # | Requirement | Feature |
|---|---|---|
| FR-1 | All tool callers dispatch through one `Tool`/`ToolContext`/`ToolRegistry`; `session.rs`'s `Tool`/`ToolRegistry` are removed. | F1,F4 |
| FR-2 | `ToolRegistry::dispatch` consults `Permissions::decide(name)` before execution; `Deny`→`is_error` refusal message, never runs. | F2 |
| FR-3 | `Ask` invokes `ctx.approval.request(...)`; allow→run, deny→`is_error` refusal. Absent sink (headless) ⇒ treat `Ask` as `Deny`. | F3 |
| FR-4 | `Session::send`/`send_text` produce byte-identical tool-loop behavior to today for the Allow path (no regression). | F4 |
| FR-5 | `ToolContext` carries `sandbox: SandboxPolicy` and `approval: Option<ApprovalSink>`; existing tools ignoring them are unaffected. | F3,F5 |
| FR-6 | A constructor binds a `SkillSet` to the registry and errors if an advertised skill has no backing tool (or vice-versa, per policy). | F6 |

**Non-functional** (measurable)

| # | Requirement | Threshold | Feature |
|---|---|---|---|
| NFR-1 | Dispatch overhead added by the gate (no Ask) | < 5µs/call (a `BTreeMap::get` + match) | F2 |
| NFR-2 | Permission default is fail-closed for `Ask` without a sink | 100% (deny) | F3 |
| NFR-3 | No behavioral regression in the existing tool/worker suites | all green | F1,F4 |
| NFR-4 | Approval round-trip is cancellation-safe | drop/SIGINT aborts the await, no orphan run | F3 |

## 5. Infra

| Need | Exists? | Where / new |
|---|---|---|
| Tool trait, registry, dispatch | ✅ | `tars-tools` (`tool.rs`, `registry.rs`) |
| Permission policy | ✅ | `tars-model` `permission.rs` (`Decision`/`Permissions`) |
| Cancellation | ✅ | `tokio_util::CancellationToken` already on `ToolContext` |
| Approval channel | ➕ | new `ApprovalSink` trait (tars-tools) + impls (TUI later, headless now) |
| Sandbox policy type | ➕ | new `SandboxPolicy` struct (seam only; lift later, Doc 22 T2) |

No new crates: `ApprovalSink`/`SandboxPolicy` live in `tars-tools`;
`Permissions` is already a `tars-model` type `tars-tools` can depend on
(or pass `Decision` in to avoid the dep — see §7).

## 6. Components

### C1 — Extended `ToolContext` (`tars-tools/tool.rs`)
- **Responsibility:** carry per-call environment + the new seams.
- **Reuses:** `tars-tools/src/tool.rs:16` (`ToolContext{cancel,cwd}`) — add two fields.
- **New:** `sandbox: SandboxPolicy`, `approval: Option<Arc<dyn ApprovalSink>>`.
- **Interface:**
  ```rust
  pub struct ToolContext {
      pub cancel: CancellationToken,         // tool.rs:21 (existing)
      pub cwd: Option<PathBuf>,              // tool.rs:26 (existing)
      pub sandbox: SandboxPolicy,            // NEW (Default = unrestricted)
      pub approval: Option<Arc<dyn ApprovalSink>>, // NEW
  }
  ```

### C2 — `ApprovalSink` (`tars-tools`, new)
- **Responsibility:** turn an `Ask` into a human allow/deny, async + cancel-aware.
- **Reuses:** mirrors the `async_trait` style of `tars-tools/src/tool.rs:144`.
- **New:** the trait + a `DenyAllSink` (headless default) + (later) a TUI sink.
- **Interface:**
  ```rust
  #[async_trait]
  pub trait ApprovalSink: Send + Sync {
      async fn request(&self, req: ApprovalRequest) -> ApprovalDecision; // Allow | Deny
  }
  pub struct ApprovalRequest { pub tool: String, pub summary: String, pub args: serde_json::Value }
  ```

### C3 — Gated dispatch (`tars-tools/registry.rs`)
- **Responsibility:** enforce permission + approval *inside* dispatch so every caller inherits it.
- **Reuses:** `tars-tools/src/registry.rs:103` (`dispatch(call, ctx) → Message`) — the place to add the gate; `:142` `execute` helper; the `is_error` Message builder at `:105–135`. Pulls the gate logic that today lives at `worker.rs:389`.
- **New:** a `PermissionView` (the `decide(name)->Decision` slice — see §7) threaded into `ToolContext` or `dispatch`’s signature; the Ask→approval branch.
- **Interface:** `dispatch` signature unchanged for callers that pass an Allow-all policy; gate reads `ctx`.

### C4 — `Session` rewire (`tars-runtime/session.rs`)
- **Responsibility:** drive the unified registry; delete the local one.
- **Reuses:** `session.rs:487` `send`, `:569` `send_text`, `:436` `register_tool`, and the dispatch loop at `:760–810` (currently calls the local `.call(args)`); replace its inner call with `tars_tools::ToolRegistry::dispatch`.
- **Removes:** `session.rs:133` `trait Tool`, `:155` `ToolRegistry`.
- **Interface:** `register_tool(Arc<dyn tars_tools::Tool>)`; `SessionOptions.tools: Option<tars_tools::ToolRegistry>`.

### C5 — `Worker` simplification (`tars-runtime/worker.rs`)
- **Responsibility:** stop hand-rolling the gate; rely on C3.
- **Reuses:** delete the inline check at `worker.rs:389`; keep the `ToolContext` build at `:401–405` (it already threads `cwd: ctx.cwd`), now also setting `sandbox`/`approval` from `AgentContext`.

### C6 — `SkillSet ↔ Registry` binding (`tars-tools` or `tars-runtime`)
- **Responsibility:** construct a registry from a `SkillSet` + tool impls, asserting no drift.
- **Reuses:** `tars-model/src/skill.rs` (`SkillSet`, `Skill{name,description}`); `registry.rs:50` `register`.
- **New:** `fn bind(skills: &SkillSet, tools: Vec<Arc<dyn Tool>>) -> Result<ToolRegistry, BindError>` — errors on advertised-but-unbacked skills.

## 7. Interfaces with other modules

| Direction | Module | Symbol / signature | Purpose |
|---|---|---|---|
| calls → | `tars-model` | `Permissions::decide(&self, &str) -> Decision` (`permission.rs`) | the gate's decision source |
| calls → | `tars-types` | `ToolCall`, `Message`, `ToolSpec` | dispatch I/O (unchanged) |
| ← used by | `tars-runtime::Session` | `ToolRegistry::dispatch`, `to_tool_specs` (`registry.rs:103/83`) | tool loop |
| ← used by | `tars-runtime::WorkerAgent` | same | tool loop |
| ← used by | Codex-TUI core (Doc 22) | `ApprovalSink` impl (TUI) | wire `Ask` to the approval widget |

**Dependency-direction decision:** to avoid `tars-tools → tars-model`, pass a
`PermissionView` (a `Fn(&str)->Decision` or a tiny trait) into `ToolContext`
rather than the concrete `Permissions`. `tars-model::Permissions` already
exposes exactly `decide()` (`permission.rs`), so the adapter is one line at
the `tars-runtime` call site. Keeps `tars-tools` leaf-level.

## 8. Main algorithms

### Gated dispatch (C3, inside `ToolRegistry::dispatch`)
```
1. decision = ctx.permission.decide(call.name)        // default Allow
2. match decision:
     Allow  -> goto 4
     Deny   -> return is_error Message("permission denied: {name}")
     Ask    -> match ctx.approval:
                 None        -> return is_error Message("denied (no approval channel)")  // fail-closed
                 Some(sink)  -> select! {
                     d = sink.request(ApprovalRequest{...}) => match d {
                         Allow -> goto 4
                         Deny  -> return is_error Message("denied by operator")
                     }
                     _ = ctx.cancel.cancelled() => return is_error Message("cancelled")   // NFR-4
                 }
4. result = execute(call, ctx)                          // existing registry.rs:142
5. return message_from(result)                          // existing registry.rs:105–135
```
Invariants: a `Deny`/`Ask`-without-sink tool is **never** executed; the
`Allow` path is identical to today (one extra `BTreeMap::get`). Edge cases:
cancellation during approval (handled); unknown tool (existing `:148`
is_error path, after the gate so even denied-unknown is consistent).

### Session rewire (C4)
Replace the local `registry.get(name).call(args)` in the `session.rs:760`
loop with `tars_tools::ToolRegistry::dispatch(call, tool_ctx)`, building
`tool_ctx` from the session's `cwd`/`cancel`/permission view — i.e. the same
construction `worker.rs:401` already does. The loop's turn-assembly
(`tool_result` blocks, `Turn::is_complete` at `session.rs:235`) is unchanged.

## 9. Integration / E2E tests

| Test | CUJ | Setup → Action → Assertion |
|---|---|---|
| E2E-1 | CUJ-1,3 | Register one `WriteFileTool` (`with_root(tmp)`); run it via **both** `Session::send` and `WorkerAgent` with `MockProvider::with_responses` emitting the same `write_file` call → both write the file, identical `ToolResult`. |
| E2E-2 | CUJ-2 | Skill `bash` = `Ask`; a scripted `ApprovalSink` returns Deny then Allow → first run produces is_error refusal (no process spawned — assert via a sentinel), second runs. |
| E2E-3 | CUJ-2 | `Ask` with `approval=None` → is_error "no approval channel"; assert tool side effect never happened (fail-closed, NFR-2). |
| E2E-4 | CUJ-2,NFR-4 | Drop the session future mid-approval (sink that never resolves) → await aborts, no execution, no panic. |
| E2E-5 | CUJ-5 | `bind(skillset{fs.edit}, [WriteFileTool])` where `fs.edit` has no backing tool → `Err(BindError::Unbacked("fs.edit"))`. |
| E2E-6 | CUJ-4 | A stub tool asserts `ctx.sandbox` is the policy passed in; default path = unrestricted, unaffected. |

## 10. Success criteria

- [ ] FR-1…FR-6 met; `session.rs` no longer defines a `Tool`/`ToolRegistry`.
- [ ] NFR-1 (<5µs gate), NFR-2 (100% fail-closed), NFR-3 (existing suites green), NFR-4 (cancel-safe).
- [ ] E2E-1…E2E-6 pass.
- [ ] Doc 21 §2 (two-registry) and §5 (`skills→tools`) marked resolved.

## 11. Performance considerations

Hot path = per-tool-call dispatch. The gate adds one `BTreeMap::get`
(`permission.rs::decide`) + a match on the Allow branch — negligible vs.
the tool's own I/O/subprocess. Approval only on `Ask` (rare, human-bound, so
latency is the human, not us). Measure: a criterion bench on `dispatch` Allow
path before/after (NFR-1). No allocation added on the Allow path.

## 12. Reliability considerations

Failure modes: (a) missing approval channel → **fail-closed deny** (NFR-2);
(b) cancellation mid-approval → `select!` on `ctx.cancel` aborts cleanly
(NFR-4, mirrors the existing `tool.rs` cancel contract); (c) sink panics →
treat as Deny (catch at the call boundary). Idempotency: the gate is pure;
dispatch already turns every outcome (miss/error/deny) into an `is_error`
`Message` (`registry.rs:105`) so the model always gets a well-formed turn —
no orphan `tool_use`. Atomic-turn rollback in `Session` (`TurnGuard`) is
unaffected.

## 13. Security considerations

Trust boundary: tool args are model-generated → untrusted. The gate is the
**chokepoint** — moving it into `dispatch` (C3) closes the current hole where
`Session`-dispatched tools run ungated. `Ask`→human is the
defense-in-depth for irreversible actions (bash/patch). `sandbox` is the seam
for OS-level confinement (Doc 22 T2). Defaults: `Permissions::default` is
permissive (`permission.rs`) — but the *runtime* should construct agents with
`deny_all().allow(...)` for untrusted contexts; document this. Approval
prompts must show the **real** command/args (no truncation that hides intent).

## 14. Abstraction & reuse

**Approach:** keep the single good abstraction (`tars_tools::Tool`) and make
everything converge on it; introduce two *seam* types (`ApprovalSink`,
`SandboxPolicy`) as `ToolContext` fields so capability grows without
re-shaping the trait. Push the existing permission *logic* (not a new policy)
down into dispatch so it's enforced once.

**Reuse map (existing code to call):**

| Symbol | Location | How we use it |
|---|---|---|
| `Tool` trait | `tars-tools/src/tool.rs:144` | the one surviving contract |
| `ToolContext` | `tars-tools/src/tool.rs:16` | extend with 2 fields |
| `ToolResult` / `ToolError` | `tars-tools/src/tool.rs:~58/~115` | unchanged dispatch I/O |
| `ToolRegistry::dispatch` | `tars-tools/src/registry.rs:103` | add the gate here |
| `ToolRegistry::{register,to_tool_specs,execute}` | `registry.rs:50/83/142` | reuse as-is |
| `Permissions::decide` / `Decision` | `tars-model/src/permission.rs` | the gate's decision source |
| permission gate pattern | `tars-runtime/src/worker.rs:389` | move down into C3, then delete |
| `ToolContext` build w/ cwd | `tars-runtime/src/worker.rs:401` | reuse for Session too |
| `Session::{send,send_text,register_tool}` | `session.rs:487/569/436` | rewire onto unified registry |
| Session tool loop | `session.rs:760–810` | swap `.call` → `dispatch` |
| `SkillSet` / `Skill` | `tars-model/src/skill.rs` | C6 binding |
| `MockProvider::with_responses` | `tars-provider` (`a769f32`) | drive E2E tool loops deterministically |
| coding builtins | `tars-tools/src/builtins/*` | already on the surviving trait — free |

**New abstractions (justified):** `ApprovalSink` (the only way to make `Ask`
real and to bridge to the TUI — Doc 21 §5 left this open); `SandboxPolicy`
(the boundary Doc 22's Seatbelt/Landlock lift attaches to — defining it now
keeps the trait stable when the lift lands); `bind()` (closes the
`SkillSet`/registry drift, Doc 21 §5). Everything else is reuse.

## Roadmap

- **M0 — `ToolContext` seam (additive, zero behavior change).** Add
  `sandbox`/`approval` fields with `Default` = unrestricted/None; add
  `ApprovalSink` + `DenyAllSink`; add `SandboxPolicy` stub. Delivers C1,C2(seam),F5.
  Depends: —. Verified by: **E2E-6** + existing suites green (NFR-3).
- **M1 — Gate inside dispatch.** Implement C3 (permission + Ask→approval +
  cancel), thread a `PermissionView` via `ToolContext`. Delivers F2,F3,FR-2,FR-3.
  Depends: M0. Verified by: **E2E-2, E2E-3, E2E-4**.
- **M2 — Session rewire / retire the fork.** C4 + C5: Session dispatches via
  `tars_tools::ToolRegistry`; delete `session.rs` `Tool`/`ToolRegistry`;
  delete the inline `worker.rs:389` gate. Delivers F1,F4,FR-1,FR-4. **Highest
  risk (touches the Session loop + every Session tool caller)** — sequenced
  here, not last, per risk-up-front. Verified by: **E2E-1** + full suite (NFR-3).
- **M3 — Skill binding.** C6 `bind()` + consistency check; resolve Doc 21 §5.
  Delivers F6,FR-6. Depends: M2. Verified by: **E2E-5**.
- **(downstream, Doc 22 T2/T3)** sandbox lift + TUI `ApprovalSink` plug into
  the M0 seams — out of scope here, unblocked by it.

Sequencing rationale: M0 is pure-additive (safe to land immediately); M1
adds enforcement behind the seam; M2 is the risky unification, done early
while context is fresh; M3 is the cleanup that the TUI/agent layer wants.
