# Antigravity (`agy`) CLI delegate — provider guide

`antigravity` is a **delegate-agent** provider: tars shells out to
Google's [`agy`](https://antigravity.google) binary, lets it run its own
coding-agent loop, and turns its printed answer into the canonical
`ChatEvent` contract. Like [`opencode`](./opencode.md) it is a black box
that reads files and runs tools, so tars wraps its process in the OS
sandbox (write-jailed to the worktree). It differs in one important way:
`agy` v1.0.16 has **no JSON output mode**, so this is the first
delegate that runs in plain-**text** output mode — the whole printed
answer becomes the response.

This is a **local/dev "bring your own agent CLI"** path, not a
production default. The spec is
[`architecture/32-cli-delegates.md`](../architecture/32-cli-delegates.md);
this is the user-facing walkthrough.

---

## 1. Installing `agy`

```
curl -fsSL https://antigravity.google/cli/install.sh | bash
```

That drops the binary at `~/.local/bin/agy`. Make sure `~/.local/bin`
is on your `PATH` (or point `executable` at the absolute path — §4).
Everything below is grounded in the installed `agy --help` for that
version.

---

## 2. What you're actually getting: a delegated agent in text mode

When tars calls antigravity it runs:

```
agy -p "<prompt>" --model <model> --dangerously-skip-permissions --add-dir <worktree>
```

- `-p "<prompt>"` — the prompt rides in the argv (there's no
  `--system-prompt` flag, so the system prompt is folded into the
  prompt as a leading `[system]` block).
- `--dangerously-skip-permissions` — `agy` is a black-box agent that
  would otherwise prompt interactively; tars runs it non-interactively.
  The safety net is **not** trust in the flag — it's the OS sandbox
  (§3) that confines whatever the agent does.
- `--add-dir <worktree>` — only appended when the request has a worktree
  cwd; it grants `agy` access to that directory.

`agy` runs its own multi-step tool loop internally. From tars's side
this is one `ChatRequest` → one `ChatEvent` stream, but inside it's an
opaque agent session — the same interchangeability caveat
[`opencode.md §1`](./opencode.md#1-what-youre-actually-getting-a-delegated-agent-not-one-inference)
raises. If you want a pure inference channel, use the HTTP backends;
`agy` is an agent by construction.

Because `agy` v1.0.16 has no `--output-format json`, it just prints the
plain answer to stdout. tars runs it in **`OutputMode::Text`**: it
drains stdout, trims the trailing newline (leading whitespace is kept —
it can be a meaningful fenced code block), and emits the whole thing as
a single `Delta`, followed by a `Finished { StopReason::EndTurn }`.
There is **no usage reported** — `agy`'s text output carries no token
counts, so `Usage` comes back as zeros rather than a fabricated number.

---

## 3. Sandboxing

`agy` is spawned through the shared `tars-sandbox` OS-jail primitive
(`build_sandboxed_command`), the same jail every CLI delegate runs in
(Doc 32 CUJ-3). It is **write-jailed to the request's worktree**, which
is also its cwd. `--dangerously-skip-permissions` disables `agy`'s own
interactive gate, and the OS sandbox is what actually keeps its file /
bash side effects confined to the worktree.

---

## 4. Authentication — passed through, not stripped

`agy` authenticates one of two ways:

- **OAuth login** — `agy`'s own interactive login session, or
- **API key env** — `GEMINI_API_KEY` or `ANTIGRAVITY_API_KEY`.

Unlike `claude_cli` / `gemini_cli` (which *strip* auth env to force a
subscription path), antigravity's env-strip table is **empty**: those
keys **pass through** the sandbox to `agy` untouched. There is no `auth`
field on the config block — tars neither manages nor injects the
credential; it just makes sure `agy`'s auth env survives into the
sandboxed process. If `agy` can't authenticate, its own error is carried
out verbatim (no sentinel).

---

## 5. Model selection

`default_model` is passed straight to `agy --model`, e.g.:

```
gemini-2.5-pro
```

tars requires the model to be explicit before the request reaches the
CLI provider — an unresolved tier hint is rejected with `InvalidRequest`
rather than guessed at.

### Prompt size cap

The prompt rides in the argv (`-p` value), so it's capped at **256 KiB**.
A larger rendered prompt is rejected up front with a clean
`InvalidRequest` rather than letting `execve` fail with `E2BIG`.

---

## 6. TOML configuration

```toml
[providers.antigravity]
type = "antigravity"
default_model = "gemini-2.5-pro"

# All optional — values shown are defaults.
executable = "agy"          # resolved on PATH; use an absolute path if ~/.local/bin isn't on PATH
timeout_secs = 300          # per-call wall clock; kills + reports on overrun

# No `auth` field. agy authenticates via its own OAuth login or via
# GEMINI_API_KEY / ANTIGRAVITY_API_KEY in the environment (passed through).
```

Only `default_model` is required. `executable` defaults to `agy`;
`timeout_secs` defaults to 300 (validated `> 0` and `<= 86_400`). These
three fields are the whole surface — `agy`'s behavior (tools, agent
loop) is owned by `agy`, not tars.

---

## 7. See also

- [`architecture/32-cli-delegates.md`](../architecture/32-cli-delegates.md) —
  the `AgentCliBackend` + `CliDialect` spec that antigravity is an impl
  of (F5); antigravity is its first `OutputMode::Text` dialect
- [`opencode.md`](./opencode.md) — the sibling delegate, in JSON event
  mode with per-step usage
- [`claude-cli.md`](./claude-cli.md) — the other subprocess provider
- Implementation: `crates/tars-provider/src/backends/cli/dialects/antigravity.rs`;
  config variant `ProviderConfig::Antigravity` in
  `crates/tars-config/src/providers.rs`
