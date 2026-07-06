# Doc 32 — CLI Delegate Backends (`AgentCliBackend` + `CliDialect`)

Status: **M0–M3 built** (all 5 dialects — claude/gemini/codex/opencode/antigravity —
on `AgentCliBackend` + uniform tars-sandbox; M4 caps/sandbox-unification pending; see
`docs/provider-layer-tracking.md` §2). Unifies the CLI-delegate providers (claude_cli /
gemini_cli / codex_cli) under a shared backend + a per-CLI behavior seam, and adds
**opencode** and **antigravity (`agy`)**. Same behavior-driven philosophy as
[Doc 30](30-openai-dialect.md) (`OpenAiDialect`), one layer up (CLI events instead
of HTTP JSON). Touches `crates/tars-provider/src/backends/`.

## 1. Overview & goal

A CLI-delegate provider shells out to a black-box coding-agent CLI (`claude -p`,
`codex exec`, `opencode run`, `agy -p`), feeds a prompt, and parses the CLI's
streamed JSON events into canonical `ChatEvent`s. Today:
- `claude_cli` — full module (`argv.rs`, `subprocess.rs` w/ `SubprocessRunner`
  trait + the tars-sandbox wrap, `streaming.rs`, `provider.rs`).
- `gemini_cli` — **reuses** claude_cli's `SubprocessRunner` (single file).
- `codex_cli` — **re-invents its own** spawn/stream loop (`codex_cli.rs:253-342`)
  instead of reusing `SubprocessRunner` — duplication.

Each backend independently hand-rolls: (a) the **argv** (per-CLI flags), and (b)
the **event parse** (per-CLI JSON schema → `ChatEvent`). That's the fragmentation.

**Goal:** one shared `AgentCliBackend` (spawn + stream + **sandbox** + prompt
plumbing) parameterized by a per-CLI **`CliDialect`** (argv + event parse). Adding
a CLI = one small `CliDialect` impl. claude/gemini/codex become impls; **opencode**
and **antigravity** are two new impls.

**Non-goals.** Not a pure-inference path — these are **delegate agents** (they run
their own tool loop), so every one is a **black box that MUST be OS-sandboxed**
(Doc 29 / `tars-sandbox`), exactly like claude_cli today. Not the product default
(the driver is neutral; production is DeepSeek/Bedrock — Doc 30/31); these are the
local/dev "bring your own agent CLI" path.

## 2. CUJs

- **CUJ-1** Add a CLI delegate = write one `CliDialect` (argv + parse) + a config
  `type`; the shared spawn/stream/sandbox is inherited.
- **CUJ-2** All existing behavior (claude/gemini/codex) preserved after they become
  `CliDialect` impls.
- **CUJ-3** A delegate's file/bash side effects are confined to the worktree by
  `tars-sandbox` (black box → OS jail), uniformly for every CLI.
- **CUJ-4** opencode: `opencode run --format json --model provider/model` → its
  newline-JSON events map to `ChatEvent`.
- **CUJ-5** antigravity: `agy -p "…" --output-format json --yes` (env auth
  `GEMINI_API_KEY`/`ANTIGRAVITY_API_KEY`) → JSON events map to `ChatEvent`.

## 3. Feature list

| # | Feature | CUJ |
|---|---|---|
| F1 | `CliDialect` trait — `argv(inv)`, `parse_line(json)->Vec<ChatEvent>`, `prompt_channel()` | 1,4,5 |
| F2 | `AgentCliBackend` — one `LlmProvider` impl: reuse `SubprocessRunner` + `tars-sandbox` + stream, delegate variance to `CliDialect` | 1,2,3 |
| F3 | Migrate claude/gemini/codex → `CliDialect` impls (codex's own spawn folds into `SubprocessRunner`) | 2 |
| F4 | `OpenCodeDialect` | 4 |
| F5 | `AntigravityDialect` | 5 |

## 4. Requirements

**Functional**

| FR | Requirement | Feature |
|---|---|---|
| FR-1 | The shared backend contains **no per-CLI branching**; argv + event parse go through `CliDialect` | F2 |
| FR-2 | `CliDialect::parse_line` maps one CLI JSON event → 0..N canonical `ChatEvent`; a shape it can't read → typed error **carrying the raw line** (CLAUDE.md #1) | F1 |
| FR-3 | **Every** CLI delegate spawns **inside `tars-sandbox`** (black box → OS jail), reusing the claude_cli wrap; fail-closed | F2 |
| FR-4 | `prompt_channel` = `Stdin` or `Arg` per CLI (claude: stdin; agy: `-p` arg / `--prompt-file`) | F1 |
| FR-5 | Migration is behavior-preserving: claude/gemini/codex produce identical events/argv | F3 |
| FR-6 | Adding a CLI touches **only** its `CliDialect` impl + a config `type` — zero edits to the shared backend | F1,F2 |

**Non-functional**

| NFR | Requirement |
|---|---|
| NFR-1 (OCP) | Shared backend closed; new CLI = new impl |
| NFR-2 (security) | Uniform OS sandbox on every delegate (Doc 29) — the delegate is untrusted/black-box |
| NFR-3 (reliability) | These consumer CLIs rate-limit / change — treat as **best-effort with fallback** in routing (ensemble), never the load-bearing path |
| NFR-4 (locality) | One CLI's full behavior (flags + parse) reads in one file |

## 5. Components

### C1 — `CliDialect` trait (`backends/cli/dialect.rs`, new)

```rust
pub trait CliDialect: Send + Sync {
    /// Executable + flags for this CLI (the per-CLI argv).
    fn argv(&self, inv: &CliInvocation) -> Vec<String>;
    /// Where the prompt goes.
    fn prompt_channel(&self) -> PromptChannel;    // Stdin | Arg | PromptFile
    /// How the CLI emits its answer — the axis that splits claude/opencode
    /// (streamed JSON events) from `agy` (a single plain-text print).
    fn output_mode(&self) -> OutputMode;          // JsonEvents | Text
    /// JsonEvents: one JSON line → 0..N canonical events (raw-carrying error).
    fn parse_line(&self, raw: &serde_json::Value) -> Result<Vec<ChatEvent>, ProviderError> {
        unimplemented!("only for OutputMode::JsonEvents")
    }
    /// Text: the whole stdout → a canonical response (default: one Delta +
    /// Finished). `agy -p` uses this.
    fn parse_text(&self, stdout: &str) -> Result<Vec<ChatEvent>, ProviderError> {
        Ok(vec![ChatEvent::delta(stdout.to_string()), ChatEvent::finished_end_turn()])
    }
    /// Optional env the CLI needs (e.g. GEMINI_API_KEY / ANTIGRAVITY_API_KEY).
    fn env(&self) -> &[&str] { &[] }
}
```
`AgentCliBackend` reads line-by-line + `parse_line` for `JsonEvents`, or drains
all stdout + `parse_text` for `Text`. The dialect declares which.

### C2 — `AgentCliBackend` (`backends/cli/mod.rs`, new)

`impl LlmProvider`. Holds `Arc<dyn CliDialect>` + `Arc<dyn SubprocessRunner>` +
`SandboxPolicy`. `stream()`: build argv via `dialect.argv`, feed prompt per
`prompt_channel`, **wrap the spawn in `tars-sandbox`** (reuse the claude_cli
`SandboxPolicy::workspace_write` path), read stdout lines, `dialect.parse_line`
each → emit `ChatEvent`.

Reuses: `SubprocessRunner`/`SubprocessInvocation` (`claude_cli/subprocess.rs` —
lift to `backends/cli/` and have claude/gemini/**codex** all use it, retiring
codex's private spawn `codex_cli.rs:253-342`); the sandbox wrap
(`claude_cli/subprocess.rs` → `tars_sandbox`); the streaming line-drain
(`claude_cli/streaming.rs`).

### C3 — Dialect impls (`backends/cli/dialects/`, new/migrated)

| Impl | argv | prompt | events |
|---|---|---|---|
| `ClaudeCliDialect` | `claude -p --model X --output-format stream-json --permission-mode bypassPermissions` | Stdin | claude stream-json |
| `GeminiCliDialect` | (today's gemini_cli argv) | (today's) | (today's) |
| `CodexCliDialect` | `codex exec --json --model X --sandbox … -` | Stdin | codex json events |
| `OpenCodeDialect` (**new**) | `opencode run --format json --model provider/model` | Arg/Stdin | `JsonEvents` — newline `{type, sessionID, …data}` |
| `AntigravityDialect` (**new**) | `agy -p "{prompt}" --model X --dangerously-skip-permissions --add-dir {worktree}` | Arg (`-p`/`--print`/`--prompt`) | **`Text`** — `agy -p` prints a plain-text answer (v1.0.16 has **no** `--output-format json`); `parse_text` → Delta+Finished. Auth: OAuth login OR `GEMINI_API_KEY`/`ANTIGRAVITY_API_KEY` env. Note `--print-timeout` (default 5m). |

> **Grounded from the installed `agy --help` (v1.0.16), not web guesses:** flags are
> `-p/--print/--prompt`, `--model`, `--dangerously-skip-permissions` (the
> bypass-permissions analogue, required for autonomous non-interactive runs),
> `--add-dir`, `--sandbox` (agy's OWN sandbox — we ignore it and wrap the process),
> `--continue`/`--conversation` (session resume). **No JSON output mode** — hence
> the `Text` output-mode axis added to `CliDialect`.

## 6. Interfaces

- **→ functional core**: unchanged — `AgentCliBackend: LlmProvider` yields canonical
  `ChatEvent`. Core never sees the CLI.
- **← config**: `ProviderConfig::{ClaudeCli, GeminiCli, CodexCli, OpenCode, Antigravity}`
  (or a unified `AgentCli { cli: enum, executable, model }`) → picks the dialect.
- **↔ tars-sandbox**: the spawn is jailed (C2); ties to Doc 29 / the shipped
  `TARS_CLAUDE_SANDBOX` wrap (generalize the env gate → per-provider policy, the
  G10 unification).

## 7. Migration / reuse map

| Symbol | file:line | Action |
|---|---|---|
| `SubprocessRunner` + `SubprocessInvocation` | `backends/claude_cli/subprocess.rs`, `argv.rs:76,104` | lift to `backends/cli/`, shared by all |
| sandbox wrap | `backends/claude_cli/subprocess.rs` (`tars_sandbox`) | move into `AgentCliBackend` — uniform for all CLIs |
| stream line-drain | `backends/claude_cli/streaming.rs` | shared |
| codex private spawn | `backends/codex_cli.rs:253-342` | **retire** → use `SubprocessRunner` |
| claude/gemini/codex argv+parse | their files | become `CliDialect` impls |
| `LlmProvider` / `ProviderRegistry` | `provider.rs:28`, `registry.rs` | one `AgentCliBackend` arm per CLI `type` |

## 8. E2E tests

- **E2E-1 (FR-5)**: claude/gemini/codex through `AgentCliBackend`+their dialect
  produce identical argv + events to today (fixture-driven, mock `SubprocessRunner`).
- **E2E-2 (CUJ-4)**: `OpenCodeDialect.parse_line` on a fixture of opencode's
  `{type,sessionID,…}` events → the expected `ChatEvent`s.
- **E2E-3 (CUJ-5)**: `AntigravityDialect` argv = `agy -p "{prompt}" --model X
  --dangerously-skip-permissions --add-dir {worktree}` (Text output-mode — v1.0.16
  has **no** JSON output, see §5 C3); parse a plain-text stdout fixture via
  `parse_text` → Delta + Finished; assert the exit-code+status double-check (a 0 exit
  with "goal not met" status → surfaced, not a false success).
- **E2E-4 (CUJ-3/FR-3)**: an `AgentCliBackend` run with a mock CLI that tries to
  write outside the worktree → blocked by `tars-sandbox` (reuse the shipped
  `security_delegate_cli.rs` pattern), for a NON-claude dialect.

## 9. Roadmap

- **M0** — `CliDialect` trait + `AgentCliBackend` extracted from claude_cli
  (`SubprocessRunner` + sandbox + stream lifted to `backends/cli/`);
  `ClaudeCliDialect` = today's behavior. Verify E2E-1 (claude identical).
- **M1** — migrate `gemini_cli` + `codex_cli` to `CliDialect` (retire codex's
  private spawn). Verify E2E-1 for all three.
- **M2** — `OpenCodeDialect` + config. Verify E2E-2 + E2E-4 (sandbox on opencode).
- **M3** — `AntigravityDialect` (`agy`) + config + env auth passthrough. Verify E2E-3.
- **M4** — fold the sandbox gate into the unified policy (G10) so `--sandbox`
  covers CLI delegates, not just `TARS_CLAUDE_SANDBOX`.

> **As-built gap (books a real structural debt).** C2 designed **one shared runner**
> (`SubprocessRunner` + sandbox + drain lifted to `backends/cli/`, every CLI reusing
> it). What shipped is **5 near-duplicate per-CLI runners** — `RealSubprocessRunner`
> (claude, `cli/subprocess.rs`), `GeminiCliSubprocessRunner`,
> `CodexCliSubprocessRunner`, `AntigravityCliSubprocessRunner`, and the opencode
> runner — each its own `impl SubprocessRunner` that spawns/drains stdout with only
> the OS-jail primitive (`build_sandboxed_command`, `cli/subprocess.rs:108`) actually
> shared (~80% of each runner is boilerplate that differs only in stdin-vs-arg prompt
> feeding and JSONL-vs-text draining). Consequences the doc must own:
> - **FR-6 is not yet true.** "Add a CLI = one `CliDialect` impl + a config `type`"
>   understates it: today a new CLI also needs its **own `SubprocessRunner` impl** and
>   a registry arm. Adding a CLI touches three places, not one.
> - Consolidating the 5 runners into the single shared spawn/drain path C2 intended
>   (dialect declares only prompt-channel + output-mode; one runner reads them) is
>   **open follow-up** — a real cleanup, not done. Booked here so it is not lost.

## 10. Notes

- **Consistency:** same behavior-driven seam as `OpenAiDialect` (Doc 30) — argv +
  parse per variant, shared core untouched. Two dialects, one philosophy, two
  layers (HTTP wire vs CLI events).
- **Security (NFR-2):** every CLI delegate is a black-box agent running its own
  tools → **must** be OS-sandboxed; tars wraps the process, never manages the CLI's
  internals (its perms/MCP are the user's config there).
- **Reliability (NFR-3):** consumer CLIs rate-limit/change/break — wire them as
  best-effort behind routing fallback; the product path stays DeepSeek/Bedrock.
