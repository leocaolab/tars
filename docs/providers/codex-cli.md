# codex CLI delegate — provider guide

`codex_cli` is a **delegate-agent** provider: tars shells out to OpenAI's
locally installed [`codex`](https://github.com/openai/codex) binary, lets
it run its own coding-agent loop, and maps codex's JSONL event stream back
onto the canonical `ChatEvent` contract. Like [`opencode`](./opencode.md)
and [`antigravity`](./antigravity.md) it is a black box that reads files
and runs tools, so tars wraps the whole process in the OS sandbox
(write-jailed to the worktree) — on top of codex's own `--sandbox`.

This is a **local/dev "bring your own agent CLI"** path, not a production
default; this is the user-facing walkthrough.

---

## 1. What you're actually getting: a delegated agent, not one inference

When tars calls codex it runs (prompt on **stdin**):

```
codex exec --json --model <model> --sandbox <mode> -c approval_policy="never" [--skip-git-repo-check] -
```

`codex exec` is a full agent invocation: around your prompt it can open
files, run shell commands, and take several internal steps before it
emits a final `agent_message`. From tars's side this is one `ChatRequest`
→ one `ChatEvent` stream, but inside it is an opaque multi-step session —
the same interchangeability caveat
[`claude-cli.md §1`](./claude-cli.md) raises, except codex has **no**
"disable the agent loop" knob. If you want a pure inference channel, use
the HTTP backends.

- `approval_policy="never"` forces non-interactive mode: any
  approval-required action fails back to the model instead of prompting.
  codex has no `--ask-for-approval` flag; the `-c` override is the
  supported path
  (`crates/tars-llm/src/provider/backends/cli/dialects/codex.rs:105-111`).
- `-` tells codex to read the prompt from stdin
  (`codex.rs:115-117`, `prompt_channel → Stdin` at `codex.rs:146-149`).

---

## 2. Model

`default_model` is passed straight through to codex's `--model` flag
(`codex.rs:104`, argv built in `build_codex_argv` at `codex.rs:93-118`).
Set it via the config field `default_model`
(`crates/tars-config/src/providers.rs:366`) or TOML
`[providers.codex_cli] default_model = "…"` (§4). A per-request model, if
supplied, overrides it.

Known model ids (`crates/tars-config/data/provider.toml:719-745`, all
`billed = "subscription"`):

| id | tier |
|---|---|
| `gpt-5.6-sol` | flagship — **default** (`provider.toml:707`) |
| `gpt-5.6-terra` | mid |
| `gpt-5.5` | flagship |
| `gpt-5.4` | mid |

`context` / `max_output` are not documented for the codex path and are
left blank in the DB (`provider.toml:717-718`).

> ⚠️ **Gotcha — your ChatGPT account must recognize the model id.** The
> id is handed verbatim to `codex exec --model` (`codex.rs:104`); tars
> does not validate it against the account. If the account no longer
> serves an id (e.g. a stale `gpt-5.x` from an older config), codex
> rejects the request and tars carries **codex's own error out verbatim**
> as a typed `CliSubprocessDied` — no sentinel substituted
> (`codex.rs:318-329`, `map_thread_event` for `turn.failed` / `error`).
> Fix by setting `default_model` to a current id your account serves.

---

## 3. Auth / credentials — subscription, self-authenticated

codex self-authenticates through its **own** ChatGPT Plus/Pro
subscription login (`codex login`) — tars threads it **no** provider
credential. The provider is wired with `Auth::Delegate`, which resolves
to `ResolvedAuth::None`: "the spawned tool/CLI carries its own credential;
tars injects nothing" (`crates/tars-llm/src/provider/auth.rs:137-139`;
`Auth::Delegate` doc at `crates/tars-types/src/providers/auth.rs:17-20`).
CLI providers consume no `Auth` (`registry.rs:511-513`).

**Stripped env** — these keys are removed from the child process
(case-insensitive) so codex's auth manager can't pick one up and route
the request through **API billing** instead of your subscription
(`codex.rs:34-38`):

- `OPENAI_API_KEY`
- `CODEX_API_KEY`
- `CODEX_AGENT_IDENTITY`

So exporting `OPENAI_API_KEY` in your shell does **not** feed codex a key
here — it is deliberately hidden to keep the subscription path active. If
codex can't authenticate, its own error surfaces verbatim (§2), not a
tars sentinel.

codex's home (`$CODEX_HOME`, default `~/.codex` — its config, sessions,
logs) is granted into the write-jail so login state survives the sandbox
(`codex.rs:161-171`).

---

## 4. TOML configuration

```toml
[providers.codex_cli]
type = "codex_cli"
default_model = "gpt-5.6-sol"   # must be an id your ChatGPT account serves

# All optional — values shown are defaults.
executable = "codex"            # resolved on PATH; use an absolute path otherwise
timeout_secs = 600              # codex runs a full agent loop → 10 min (vs 5 for others)
sandbox = "read-only"           # codex's INTERNAL sandbox: "read-only" | "workspace-write" | "danger-full-access"
skip_git_repo_check = true      # codex's git-repo gate rejects tempdir/scratch runs otherwise

# No `auth` field. codex authenticates via its own `codex login`
# (ChatGPT subscription); OPENAI_API_KEY / CODEX_API_KEY / CODEX_AGENT_IDENTITY
# are stripped from the child so they can't divert it onto API billing.
```

Only `default_model` is required. Field grounding:
`executable` (`providers.rs:348`, default `"codex"` at `providers.rs:439`),
`timeout_secs` (`providers.rs:352`, default 600 at `providers.rs:455`),
`sandbox` (`providers.rs:358`; `CodexSandboxConfig` kebab-case wire values
`read-only` / `workspace-write` / `danger-full-access` at
`providers.rs:472-478`, default `read-only`),
`skip_git_repo_check` (`providers.rs:364`, default `true`).

`sandbox` is codex's **own** internal sandbox; the `tars-sandbox` OS jail
wraps the process on top of it (defense-in-depth — the `--sandbox` token
stays in the argv, `codex.rs:11-14`).

---

## 5. See also

- [`opencode.md`](./opencode.md), [`antigravity.md`](./antigravity.md)
  — the sibling delegates
- [`claude-cli.md`](./claude-cli.md) — the other stdin/JSON subprocess provider
- Implementation:
  `crates/tars-llm/src/provider/backends/cli/dialects/codex.rs`; config
  variant `ProviderConfig::CodexCli` in
  `crates/tars-config/src/providers.rs`
