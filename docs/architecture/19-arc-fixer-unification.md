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

## 5. Plan

1. **Coding builtins** — `fs.write_file` / `fs.edit_file` / `bash.run`.
   ✅ Done (`3784992`).
2. **Agent interface** — promote `Worker` to `Agent` (skills/permissions/
   config); native (`Session`) + user impls conform.
3. **Agent-level routing/ensemble** — `EnsembleAgent`/`RoutingAgent` over
   `Agent`.
4. **arc fixer → tars-native coding agent** — `Session` + the new tools
   over `claude_cli` (Disabled = pure inference). Delete arc's `claude.rs`.
5. tars user-guide + README; arc cleanup; dogfood.

## 6. What this is NOT

- NOT "tars lacks an agent layer" — it has `Session` + `Worker` +
  `tars-tools` + a Doc 04/05 spec for agent/tools/skills/permissions. This
  completes + lifts them, it doesn't invent them.
- NOT forcing the black-box CLI loop. The CLI is a pure-inference provider;
  tars owns the loop. (A user may still bring a black-box agent — the
  adaptor takes it — but it's a choice, not the default.)
