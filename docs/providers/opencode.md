# opencode CLI delegate — provider guide

`opencode` is a **delegate-agent** provider: tars shells out to your
locally installed [`opencode`](https://opencode.ai) binary and lets it
run its own coding-agent loop, then maps opencode's JSON event stream
back onto the canonical `ChatEvent` contract. Unlike the
[`anthropic`](./anthropic.md) HTTP backend (a pure inference channel)
or even [`claude_cli`](./claude-cli.md) with tools disabled, opencode
is **always** a black-box agent — it reads files, runs tools, and takes
its own internal turns. tars wraps that whole process in the OS sandbox
so its side effects stay inside the worktree.

This is a **local/dev "bring your own agent CLI"** path, not a
production default. The spec is
[`architecture/32-cli-delegates.md`](../architecture/32-cli-delegates.md);
this is the user-facing walkthrough.

---

## 1. What you're actually getting: a delegated agent, not one inference

When tars calls opencode it runs:

```
opencode run --format json --model <provider/model> "<prompt>"
```

`opencode run` is a full agent invocation. Around your prompt it can
open files, grep the tree, edit code, and run shell commands across
several internal steps before it emits a final answer. From tars's side
this looks like one `ChatRequest` → one `ChatEvent` stream, but on the
inside it is an opaque multi-step session you didn't explicitly ask for
— the same interchangeability caveat that
[`claude-cli.md §1`](./claude-cli.md#1-the-surprise-claude_cli-is-not-a-pure-inference-channel-by-default)
raises, except here there is **no** "disable the agent loop" knob.
opencode is an agent by construction; if you want a pure inference
channel, use the HTTP backends.

Because it's a black box that touches the filesystem, opencode is
spawned through the shared `tars-sandbox` OS-jail primitive
(`build_sandboxed_command`): it is **write-jailed to the request's
worktree**, which is also set as its cwd (opencode picks up the project
from `process.cwd()`). This is the same jail every CLI delegate runs
in (Doc 32 CUJ-3).

Practically, opencode sits **behind routing fallback**: it's a
best-effort delegate you reach for when you want a real coding agent
locally, not the neutral inference path that production routing prefers.

---

## 2. Model selection — opencode's `provider/model` form

`default_model` is passed straight through to opencode's `--model`
flag, so it must be in **opencode's own `provider/model` spelling**, not
a bare model id:

```
anthropic/claude-sonnet-4-5
openai/gpt-5
google/gemini-2.5-pro
```

tars requires the model to be explicit before the request reaches the
CLI provider — a tier hint (`ModelTier::Default`) that never got
resolved to a concrete id is rejected with `InvalidRequest` rather than
guessed at. Whatever you put in `default_model` (or an explicit
per-request model) is what opencode receives.

---

## 3. Authentication — opencode's, not tars's

tars **does not manage opencode's credentials**. opencode authenticates
however you configured it on its own side:

- `opencode auth login` (its subscription / OAuth flow), or
- provider API keys in opencode's own environment/config.

The dialect's env-strip table is **empty** — tars neither injects nor
strips auth env for opencode, it just spawns the binary and lets
opencode resolve its own credentials. There is no `auth` field on the
`opencode` config block; if opencode can't authenticate, the failure
surfaces as opencode's own error (see §5), carried out verbatim.

---

## 4. TOML configuration

```toml
[providers.opencode]
type = "opencode"
default_model = "anthropic/claude-sonnet-4-5"   # opencode's provider/model form

# All optional — values shown are defaults.
executable = "opencode"     # resolved on PATH; override with an absolute path
timeout_secs = 300          # per-call wall clock; kills + reports on overrun
```

Only `default_model` is required. `executable` defaults to `opencode`
(found on `PATH`); `timeout_secs` defaults to 300 (validated to be
`> 0` and `<= 86_400`). These three fields are the entire surface —
opencode has no builder-level knobs the way `claude_cli` does, because
its behavior (tools, memory, agent loop) is owned by opencode itself.

---

## 5. Event mapping and the honest caveats

opencode's `--format json` stream is newline-delimited JSON; each line
is one `{ type, timestamp, sessionID, ...data }` object. tars maps them
as follows:

| opencode `type` | canonical `ChatEvent` | notes |
|---|---|---|
| `text` | `Delta { text }` | `part.text` is a **complete** assistant text block (opencode only emits it once `part.time.end` is set) |
| `reasoning` | `ThinkingDelta { text }` | the thinking text |
| `step_finish` | *(accumulated into usage)* | `part.tokens` → summed into the terminal `Finished` |
| `step_start`, `tool_use` | *(ignored)* | no canonical content to surface |
| `error` | typed error, **raw carried out** | see below |

Two caveats are baked into the source and worth stating plainly, because
they are honest limits of what opencode's JSON exposes:

1. **No terminal event → `EndTurn` is synthesized.** opencode's JSON
   mode has no explicit finish event; the process just exits when the
   session goes idle (`session.status: idle` is not written to stdout).
   So tars synthesizes the terminal `Finished` after the last line,
   **always with `StopReason::EndTurn`** — opencode's stream doesn't
   surface a distinct finish reason, so tars does not invent one.

2. **Usage is summed across `step_finish` parts.** An agentic turn can
   span several steps, each reporting its own `{ input, output,
   reasoning, cache: { read, write } }`. tars sums them into one
   `Usage`. For the common single-step `opencode run` there is exactly
   one `step_finish`, so the sum is exact. Whether multi-step per-step
   tokens are cumulative or per-step is not 100% pinned from opencode's
   source — summing is chosen because it never *loses* tokens.

On an `error` line, tars does **not** substitute a sentinel: it carries
opencode's raw session-error payload out inside a typed
`CliSubprocessDied` error (e.g. an auth error keeps its
`"missing api key"` message). Unparseable lines are skipped with a
debug trace rather than aborting the whole turn — opencode may add
fields or event types tars doesn't model yet.

### Prompt size cap

The prompt rides in the argv (positional `message` arg), so it's capped
at **256 KiB**. A larger rendered prompt is rejected up front with a
clean `InvalidRequest` rather than letting `execve` fail with `E2BIG`.

---

## 6. See also

- [`architecture/32-cli-delegates.md`](../architecture/32-cli-delegates.md) —
  the `AgentCliBackend` + `CliDialect` spec that opencode is an impl of
  (F4); the shared spawn / stream / sandbox machinery
- [`antigravity.md`](./antigravity.md) — the sibling delegate, in
  plain-text output mode
- [`claude-cli.md`](./claude-cli.md) — the other subprocess provider,
  which *can* be reduced to a pure inference channel (opencode can't)
- Implementation: `crates/tars-provider/src/backends/cli/dialects/opencode.rs`;
  config variant `ProviderConfig::Opencode` in
  `crates/tars-config/src/providers.rs`
