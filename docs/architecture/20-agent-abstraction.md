# Doc 20 — What is an Agent (user perspective)

> The Agent abstraction, defined from the USER's perspective (not from the
> plumbing up). Derived in discussion 2026-06; supersedes the loose use of
> "agent" scattered across `tars-runtime` (`trait Agent`, `WorkerAgent`,
> `LlmWorker`, `Session` — none of which is an Agent in this sense).

## 1. Definition

> **An Agent is a collection of capabilities (skills) that a user can hand
> a task to.**

Two things define it, both user-facing:
- **skills** — what it can do (its capability set). This is what the Agent
  *is*.
- **run(task)** — you give it a task; it does it.

Nothing else is part of the user-facing definition. How it runs internally
(an LLM loop, a subprocess, a human, anything) is an implementation detail.

## 2. What is NOT an Agent

Everything `tars-runtime` currently calls "agent"-ish is BELOW this — it's
plumbing a native Agent happens to use, not the Agent itself:

| Thing | What it really is |
|---|---|
| `trait Agent` (agent.rs, `execute(ChatRequest)`) | **a single CALL / step** — misnamed. One LLM round-trip, not an agent. |
| `Session` | the **multi-turn tool loop** — internal mechanism |
| `LlmWorker` / `Worker` | a **run_plan execution unit** — an implementation |
| `WorkerAgent` | **config** (model + tool registry) |

So the user's Agent is a NEW, top-level abstraction that *uses* these but
is none of them.

## 3. The interface

```rust
trait Agent {
    fn skills(&self) -> &SkillSet;                       // what it can do — defines it
    async fn run(&self, task: Task, ctx: AgentContext)   // you give it a task
        -> Result<AgentOutput, AgentError>;
}
```

- Takes a **Task**, NOT a `ChatRequest`. A request is LLM-message-level —
  the implementation detail of how a *native* agent turns a task into LLM
  calls. Putting `ChatRequest` on the interface (as the existing
  `trait Agent` does) leaks the LLM implementation into the user-facing
  contract. A user agent that doesn't even use an LLM should never see a
  `ChatRequest`.
- `ctx` (AgentContext) carries the ENVIRONMENT — cwd, cancel, runtime/event
  sink, permissions. "Where / how", separate from the task's "what".

## 4. Task — the recursive unit of intent

```rust
struct Task {
    goal: String,        // what to accomplish (from user input, or derived)
    inputs: ...,         // relevant context/data (a file, a PR diff, upstream findings)
    // optional: acceptance / constraints — what "done" means
}
```

- Originates from **user input** (the top-level goal), but is NOT the raw
  input — it's the structured unit handed to an Agent.
- **Recursive**: an orchestrator (or a parent agent) decomposes a top-level
  Task into sub-Tasks for sub-agents. Every layer can mint a Task; the user
  is just the root.

```
user input ──generate──> top Task ──decompose──> sub Task ──> sub-sub Task ...
                          each = one unit of work handed to one Agent
```

- NOT messages. The `Task → prompt → ChatRequest` translation is a *native*
  agent's internal job.

## 5. Two implementers, one interface (tars = adaptor)

- **native agent** — built on an LLM: `run(task)` internally turns the task
  into prompts and drives a `Session` loop over a pure-inference provider +
  its skills' tools. "gemini agent" / "claude_cli agent" = the same native
  machinery, different provider. White-box (tars owns the loop, tools,
  permissions, observability).
- **user agent** — the user implements `run(task)` however they want.

tars defines the `Agent` interface + the orchestration over it
(routing/ensemble/run_plan/events/permissions) and **adapts both**.

## 6. Layering

```
        Agent  { skills, run(task) }      ← THE agent (top, user-facing)
   ┌──────────┴───────────┐
 native                  user
   │ run(task) internally uses:
   ├ Session (loop)
   ├ Worker / LlmWorker (run_plan unit)
   ├ trait-Agent-today = "a call" (rename)
   ├ Provider (pure inference: claude_cli --bare / Disabled, gemini, …)
   └ Tools (read/write/edit/bash — shipped)
```

## 7. Open (to settle before coding the trait)

- Exact `Task` fields (does it carry acceptance criteria? a parent/lineage
  for the recursive case?).
- Where skills / permissions / config sit: methods on the trait
  (`skills()` is in §3) vs the AgentContext vs config.
- The existing single-step `trait Agent` rename ("Call" / "Step") to free
  the name `Agent` for this.
