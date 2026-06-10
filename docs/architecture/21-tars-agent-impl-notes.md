# Doc 21 — Native agent implementation notes + open decisions

> Working notes from implementing the native `Agent` (the LLM-backed
> implementer of `tars_model::Agent`). The native agent now exists
> (`TarsAgent` + `EnsembleAgent`, §1); what remains are the cleanups it
> surfaced in tars's tool machinery (§2) and the follow-ons in §5. The
> §3 loop decision is **resolved** — see below.

## 1. What's done

- `tars-model` (Doc 20) — `trait Agent { id, role, skills, run(task) }` +
  `Task` / `SkillSet` / `Permissions` / `AgentContext` / `AgentOutput`.
  Pure, depends only on `tars-types`. Committed (`e66a73c`).
- `tars-tools` coding builtins — `fs.write_file` / `fs.edit_file` /
  `bash.run`, all reading `ToolContext::cwd`. Committed (`3784992`).
- **cwd seam** (§4) — `AgentContext` gained `cwd: Option<PathBuf>`; the
  WorkerAgent tar-tools loop builds each `ToolContext` from it
  (`worker.rs:403`) instead of hardcoded `None`. Committed (`767bd8c`,
  CLI site `8e63232`).
- **`TarsAgent`** (`tars-runtime/tars_agent.rs`) — the LLM-backed
  implementer of `tars_model::Agent`. Hand it a `Task`; it renders the
  task into a single instruction and drives the existing **WorkerAgent**
  tars-tools loop over a pure-inference provider, threading `ctx.cwd` so
  tools act on the worktree. Swapping the provider is what makes it a
  "gemini agent" vs a "claude_cli agent". This is **option A** of §3 —
  reuse the WorkerAgent loop via a synthetic one-step `Plan`. Committed
  (`73fefd1`; renamed `NativeAgent → TarsAgent` in `1ab096b`).
- **`EnsembleAgent`** (`tars-runtime/ensemble_agent.rs`) — agent-level
  hedging: runs one `Task` on N candidate `Agent`s concurrently, returns
  the first success, cancels the rest. Composes over the `Agent` trait, so
  it's blind to native vs user candidates. Committed (`653abd8`).
- **Permission enforcement** at tool dispatch (`3d6a79f`) — see §5.
- Test support: `MockProvider::with_responses` queues per-call replies so
  a multi-turn agent loop can be driven deterministically in tests
  (`a769f32`).

> ⚠️ Known limitation of the option-A reuse: `WorkerAgent` parses the
> model's FINAL turn as a `{summary, confidence}` worker result, so a
> `TarsAgent`'s last message must be that shape. Fine for "do X and
> report"; a freer output contract is a follow-on (§5).

## 2. The blocker: TWO tool systems named `ToolRegistry` — ✅ RESOLVED (Doc 23 M2)

> The fork below is **gone**: the Session-local `Tool`/`ToolRegistry` were
> deleted and `Session` now dispatches through `tars_tools::ToolRegistry`
> (Doc 23, `ec90f4d`). The history is kept for context.

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

## 3. Open decision — which loop backs the tars agent?

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
- **C. Purpose-built tars-agent loop.** A small new loop (in
  `tars-runtime` or a `tars-agent` crate): take `Task` + `AgentContext`,
  call the `LlmService` in a loop, dispatch `tars_tools` tools with
  `ToolContext{ cwd: ctx.cwd, cancel: ctx.cancel }`, return `AgentOutput`.
  Reuses the tars-tools registry (cwd-aware) + the provider, NOT the
  WorkerAgent/Session plumbing. Fastest to a WORKING native coding agent;
  cost = a third loop unless we later collapse A/B into it.

**RESOLVED — shipped option A (`TarsAgent`, `73fefd1`).** Rather than add
a third loop (C) or take the big Session refactor (B) up front, `TarsAgent`
reuses the existing WorkerAgent tars-tools loop by shaping the `Task` into a
synthetic one-step `Plan`. This got a working, cwd-aware, provider-agnostic
native coding agent fastest, with zero new loop. The cost is the
awkward-fit noted above (the `{summary, confidence}` final-turn contract
leaks from WorkerAgent) and the still-open two-`ToolRegistry` fork (§2).
**B remains the eventual end state**: unify Session's tools onto tars-tools,
retire the second registry, and collapse onto one loop. Tracked in §5.

## 4. The cwd seam (independent of the above, clearly correct) — ✅ DONE

Regardless of A/B/C, the WorkerAgent tar-tools loop should be able to act
on a worktree. The bounded fix landed (`767bd8c`): `AgentContext` carries
`cwd: Option<PathBuf>` and the loop builds each `ToolContext` from it
(`worker.rs:403`) instead of `None`. Additive + safe — `run_plan` sites
pass `None` (unchanged behaviour), and any tars-tools-based agent loop can
now act on a scoped tree. This is what `TarsAgent` threads through.

## 5. Open questions (status)

- ✅ **`Permissions` enforcement** — DONE (`3d6a79f`). The WorkerAgent tool
  loop checks `ctx.permissions.is_allowed(tool)` before dispatch; a
  Deny/Ask skill yields an `is_error` result, never running. (`Ask` is
  treated as a refusal until a human-prompt channel exists.)
- ✅ The two-`ToolRegistry` fork (§2) — **RESOLVED** (Doc 23 M2, `ec90f4d`).
  The Session-local `Tool`/`ToolRegistry` are deleted; `Session` dispatches
  through `tars_tools::ToolRegistry::dispatch`, the same gated path the
  WorkerAgent uses.
- ✅ `skills() → tools` binding — **RESOLVED** (Doc 23 M3, `08d5447`).
  `tars_runtime::bind(&SkillSet, tools)` builds the registry and rejects any
  advertised skill with no backing tool (`BindError::Unbacked`), so skills
  and tools can't drift.
- ✅ `Ask` permission — **RESOLVED** (Doc 23 M0/M1, `ad21e44`). The dispatch
  gate consults a `ToolContext::approval` sink for `Ask` (allow/deny, races
  `cancel`); with no sink it fails closed (== Deny, as before). The TUI's
  approval widget is the sink (Doc 22).
- ⬜ `AgentRole` / `trait Agent` duplication: `tars-model::AgentRole` vs the
  one in `tars-runtime/agent.rs` (the single-call thing). Unify by having
  runtime re-export the model's, and rename runtime's `trait Agent` →
  `Call`/`Step` to free the name.
