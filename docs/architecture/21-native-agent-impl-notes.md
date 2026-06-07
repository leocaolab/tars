# Doc 21 — Native agent implementation notes + open decisions

> Working notes from implementing the native `Agent` (the LLM-backed
> implementer of `tars_model::Agent`). Records a real fork in tars's tool
> machinery that has to be resolved before the native agent is clean.
> Captured for the team to decide — see §3.

## 1. What's done

- `tars-model` (Doc 20) — `trait Agent { id, role, skills, run(task) }` +
  `Task` / `SkillSet` / `Permissions` / `AgentContext` / `AgentOutput`.
  Pure, depends only on `tars-types`. Committed (`e66a73c`).
- `tars-tools` coding builtins — `fs.write_file` / `fs.edit_file` /
  `bash.run`, all reading `ToolContext::cwd`. Committed (`3784992`).

## 2. The blocker: TWO tool systems named `ToolRegistry`

A native coding agent needs to run a multi-turn LLM↔tool loop whose tools
act on the agent's worktree (`AgentContext::cwd`). tars has TWO tool-loop
implementations, and they use DIFFERENT, incompatible tool registries:

| loop | registry | tool call shape | cwd? |
|---|---|---|---|
| **`WorkerAgent`** (`tars-runtime/worker.rs`) | `tars_tools::ToolRegistry` (Doc 05) | `Tool::execute(args, ToolContext{cwd,cancel})` | **yes** — but hardcoded `cwd: None` at `worker.rs:384` |
| **`Session`** (`tars-runtime/session.rs`) | a SECOND `ToolRegistry` defined inside `session.rs` (line ~155) | `tool.call(args)` | **no** — no `ToolContext` at all |

So:
- The coding builtins are `tars-tools` (`ToolContext::cwd`). Only the
  **WorkerAgent** path can run them. The **Session** path uses a different
  tool abstraction (`.call(args)`, no cwd) — the coding tools don't even
  fit it.
- Even on the WorkerAgent path, `cwd` is hardcoded `None`
  (`worker.rs:384`), so tools currently act on the process cwd, not a
  worktree. `WorkerContext` (`executor.rs:100`) carries `cancel` but no
  `cwd`.

This fork is pre-existing tech debt, surfaced (not caused) by the native
agent work.

## 3. Open decision — which loop backs the native agent?

Three options:

- **A. WorkerAgent + synthetic plan/step.** Reuse `worker.rs`'s tars-tools
  loop. Cost: `WorkerAgent` is `Worker::run(plan, step, prior, ctx)` —
  run_plan-shaped; driving it for a single ad-hoc Task means faking a
  one-step plan. Awkward fit.
- **B. Session, after unifying its tools onto tars-tools.** Make
  `Session` use `tars_tools::ToolRegistry` + `ToolContext` (retire the
  in-`session.rs` registry). Cleanest END state (one tool system, the nice
  `send_text` API), but a bigger refactor touching the Session tool loop +
  every Session tool caller.
- **C. Purpose-built native-agent loop.** A small new loop (in
  `tars-runtime` or a `tars-agent` crate): take `Task` + `AgentContext`,
  call the `LlmService` in a loop, dispatch `tars_tools` tools with
  `ToolContext{ cwd: ctx.cwd, cancel: ctx.cancel }`, return `AgentOutput`.
  Reuses the tars-tools registry (cwd-aware) + the provider, NOT the
  WorkerAgent/Session plumbing. Fastest to a WORKING native coding agent;
  cost = a third loop unless we later collapse A/B into it.

**Recommendation: C now, B eventually.** C gets a working, cwd-aware,
provider-agnostic native coding agent quickly without the messy fit (A) or
the big refactor (B), and it's the natural shape the `Agent::run(task)`
contract wants. Then fold Session's tools onto tars-tools (B) and, if it
makes sense, have Session reuse C's loop — converging on one tool system +
one loop. **This needs a yes before building C** (it adds a third loop
temporarily); flagged here rather than committed.

## 4. The cwd seam (independent of the above, clearly correct)

Regardless of A/B/C, the WorkerAgent tar-tools loop should be able to act
on a worktree. The bounded fix: add `cwd: Option<PathBuf>` to
`WorkerContext` and use it at `worker.rs:384` instead of `None`. This is
additive + safe and unblocks ANY tars-tools-based agent loop from acting on
a scoped tree. (Doing this next.)

## 5. Open questions parked here (per "record issues in docs")

- The two-`ToolRegistry` fork (§2) — is the long-term plan to unify on
  `tars_tools::ToolRegistry`? (Assumed yes; Session's is the odd one out.)
- `skills() → tools` binding: a native agent's `SkillSet` must map to the
  concrete `tars_tools` tools it exposes. Where does that mapping live — a
  registry from skill-name → Tool, in `tars-runtime`? (Likely a
  `tars-agent` assembly crate.)
- `Permissions` enforcement point: gate at tool-dispatch (filter the
  registry / reject a call whose skill is `Deny`/`Ask`). The native loop
  consults `ctx.permissions` before each `dispatch`. Not yet wired.
- `AgentRole` duplication: `tars-model::AgentRole` vs the one still in
  `tars-runtime/agent.rs` (the single-call thing). Unify by having runtime
  re-export the model's, and rename runtime's `trait Agent` → `Call`/`Step`.
