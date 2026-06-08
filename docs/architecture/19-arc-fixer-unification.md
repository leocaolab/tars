# Doc 19 — Agent layer: provider-agnostic, white-box agents

> Scope: the Agent layer that lets us build white-box, provider-agnostic
> coding agents ("gemini agent", "claude_cli agent") on tars's OWN tool
> loop — and adapt user-supplied agents through the same interface —
> instead of depending on a CLI's internal black-box loop. Replaces arc's
> private `claude.rs` fixer.
>
> Status: in progress. Coding builtins (`fs.write_file`, `fs.edit_file`,
> `bash.run`) landed; the Agent interface is next.

## 1. The split: Provider (reasoning) vs Agent (action)

Two layers, kept clean:

| | **LlmProvider** | **Agent** |
|---|---|---|
| contract | `ChatRequest → token stream` | `task + working context → result + side effects` |
| state | stateless | stateful (session/history) |
| tools | none | uses tools (read/write/edit/bash/skill) |
| side effects | none | edits files, runs commands |
| working context | none | a `cwd` it acts on |
| permissions | none | what it's allowed to do |
| loop | single completion | multi-turn agentic loop |

An **Agent USES a provider** for each reasoning step and adds tools, state,
side-effects, a working context, permissions, and the loop. The provider is
the swappable brain; the Agent is the actor.

## 2. The key choice: tars runs the loop (white box), not the CLI

The claude CLI can run its OWN agentic loop (its internal Read/Edit/Bash +
planning). Using that makes it a **black box**: you can't see/control which
tools it calls, can't enforce permissions, can't observe or route the inner
steps, can't swap the model mid-loop.

Instead we use the coding CLI as a **pure-inference provider** —
`claude_cli` with `ClaudeCliTools::Disabled` (`--tools ""`) is exactly that:
"a pure inference channel, auth-neutral" — and run the loop in **tars's own
Session** (`tars-runtime`), driving tars's own tools (`tars-tools`:
read/write/edit/bash). Now:

- tars controls the tools, permissions (Doc 05 IAM), and observability —
  **white box**.
- The provider is swappable → `gemini agent` and `claude_cli agent` are the
  SAME Session loop over different pure-inference providers.
- The agent's side effects land in `ToolContext::cwd` (the worktree the
  orchestrator scoped), set by the Session, NOT the provider.

## 3. tars is an ADAPTOR — native AND user agents

Agents come in two kinds, both conforming to one interface:

- **tars-native** — built on an LLM: a `Session` loop over a provider +
  `ToolRegistry`. tars runs it.
- **user-implemented** — the user brings their own agent/loop (e.g. arc's
  current `ClaudeFixerWorker`, or a CLI's internal loop if they want it).

tars defines the `Agent` interface + the orchestration (routing, ensemble,
run_plan, events, permissions) and **adapts both**. tars's existing
`Worker` trait already IS this adaptor seam (anyone implements it;
`LlmWorker`/`Session` is native, arc's `ClaudeFixerWorker` is a user one).
The work is to promote `Worker` into a clean `Agent` (skills / permissions /
config as first-class) and lift routing/ensemble to operate over it.

## 4. Layer placement

```
tars-pipeline (LlmService onion: provider routing/ensemble OVER COMPLETIONS)
tars-tools    (Tool + registry + builtins: read/write/edit/bash)   ← shipped
        └────────────┬───────────────┘
                     ▼
            Agent layer  (trait Agent: config + skills + permissions + run)
            ├ native: Session(provider --bare/Disabled + ToolRegistry) — gemini/claude_cli agents
            ├ user:   any impl of the interface
            └ EnsembleAgent / RoutingAgent  ── routing/ensemble OVER AGENTS (task granularity)
                     ▼
            tars-runtime (Worker = run an Agent for a step; run_plan = the DAG)
```

Two routing granularities, no conflict: **completion-level** stays in
`tars-pipeline` (one text completion across providers); **task-level** is
the Agent layer (a whole task across agents — `gemini agent` vs
`claude_cli agent` vs a user agent). The tool-using fixer needs the latter.

## 5. Plan — status

1. **Coding builtins** — `fs.write_file` / `fs.edit_file` / `bash.run`.
   ✅ Done (`3784992`).
2. **Agent interface** — `tars-model` crate: `trait Agent { id, role,
   skills, run(task) }` + `Task`/`SkillSet`/`Permissions`/`AgentContext`.
   ✅ Done (`e66a73c`). (Note: it's a NEW top-level abstraction, not a
   promotion of `Worker` — see Doc 20. The old single-call `trait Agent`
   stays as plumbing.)
3. **TarsAgent** — LLM-backed `tars_model::Agent`; task → white-box tool
   loop over a pure-inference provider; cwd threaded model→tool. ✅ Done
   (`73fefd1`, e2e tested). cwd seam: `767bd8c`.
4. **EnsembleAgent** — agent-level hedge (first success wins). ✅ Done
   (`653abd8`).
5. tars user-guide + README — ✅ Done. arc cleanup; dogfood — open.
6. **arc fixer → tars-native coding agent** — NOT done; risky to execute
   pre-release while the dogfood runs. Plan documented in §7 below;
   review before executing.

## 6. What lands a working coding agent today

```rust
let agent = TarsAgent::new("agent:fixer", "fix", skills, model, llm /*pure inference*/, tools);
let out = agent.run(Task::new(id, goal), AgentContext::new().with_cwd(&worktree)).await?;
```
Hedge: `EnsembleAgent::new("ens", role, vec![claude_cli_agent, gemini_agent])`.

## 7. arc integration plan (documented, NOT executed)

The remaining step — arc's fixer stops using its private `claude.rs`
black-box loop and becomes a `tars_model::Agent`:

1. arc bumps its `tars` git rev to one with the agent layer (≥ `653abd8`).
2. arc builds a `TarsAgent` for the fixer role: a `ToolRegistry` of
   `WriteFileTool`/`EditFileTool`/`BashTool` jailed to the fix worktree, a
   `claude_cli` provider in `ClaudeCliTools::Disabled` (pure inference),
   the fixer model. Optionally wrap N providers in an `EnsembleAgent`.
3. arc's fixer call site (`agent_backend`) builds a `Task` from the finding
   (goal = the fix instruction, inputs = file/snippet/critic history,
   acceptance = "compiles + the finding is resolved") and calls
   `agent.run(task, ctx.with_cwd(worktree))`.
4. Map `AgentOutput` back into arc's fix-loop result; the worktree diff is
   read as today.
5. Delete `arc_shell/src/agent_backend/claude.rs` — now redundant.

Risks to weigh first (why this is documented, not done): it's a core change
to a pre-release fixer with a long dogfood run in flight; the
`{summary,confidence}` final-turn contract (Doc 21) may not fit arc's fixer
prompt; and the two-tool-systems fork (Doc 21 §2) is still open.

## 6. What this is NOT

- NOT "tars lacks an agent layer" — it has `Session` + `Worker` +
  `tars-tools` + a Doc 04/05 spec for agent/tools/skills/permissions. This
  completes + lifts them, it doesn't invent them.
- NOT forcing the black-box CLI loop. The CLI is a pure-inference provider;
  tars owns the loop. (A user may still bring a black-box agent — the
  adaptor takes it — but it's a choice, not the default.)
