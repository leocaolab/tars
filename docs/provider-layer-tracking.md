# Provider Layer — Tracking Doc

Living checklist for the provider-layer evolution. Watches THREE efforts so
neither stalls. Companion designs: [Doc 30](architecture/30-openai-dialect.md)
(OpenAiDialect), [Doc 31](architecture/31-bedrock.md) (Bedrock), [Doc 32](architecture/32-cli-delegates.md)
(CLI delegates).

Status: ✅ done · 🚧 in progress · ⬜ not started · ⚠️ gap.

---

## 0. Goal & the two axes

tars is a **provider-agnostic driver**. Two behavior-driven seams, same
philosophy (shared core + per-variant behavior, OCP), different layers:

| Layer | seam | shared core | variants |
|-------|------|-------------|----------|
| **HTTP wire** (OpenAI-compat) | `OpenAiDialect` (Doc 30) | the `openai` adapter/mapping | DeepSeek, LM Studio, Groq, vLLM, MLX, … |
| **CLI events** (delegate agents) | `CliDialect` (Doc 32) | `AgentCliBackend` (spawn+sandbox+stream) | claude/gemini/codex + opencode/antigravity |
| **Different protocol** | own `LlmProvider` | — | Anthropic, Gemini, **Bedrock** (Doc 31) |

Production path = DeepSeek (OpenAiDialect) + Bedrock/Vertex (cloud). CLI delegates
= local/dev "bring your own agent CLI", best-effort behind routing fallback.

## 1. Effort A — OpenAiDialect (Doc 30) — **~DONE**

- **M0** ✅ `OpenAiDialect` trait + `StandardDialect` (defaults delegate to
  today's adapter/mapping) + backend holds `Arc<dyn OpenAiDialect>`. 186 tests, no
  behavior change.
- **M1** ✅ `DeepSeekDialect` — the ONE real per-provider branch (top-level
  `thinking:{type}`) moved out of `build_request_default`; base_url inference
  selects it (behavior byte-for-byte). `grep base_url.contains("deepseek")` in the
  body builder → gone; only the single selection point remains (`provider.rs:99`).
  189 tests green.
- **M2 (LmStudio)** ❌ **moot** — `response_format` (json_schema vs json_object) is
  already config-driven via `structured_mode: StructuredOutputMode` (the
  routing-relevant place). Nothing to extract.
- **FR-5** ✅ — the 7 openai parse-failure sites (`mapping.rs`) now append the raw
  payload (truncated 300 via `http_base::truncate`, UTF-8-safe + ellipsis); type
  unchanged (`ProviderError::Parse`) so no ripple. 4 new tests prove the raw is
  carried + capped. 193 tests green.
- **Remainder (minor, do on demand):**
  - ⬜ **M3 polish**: an explicit optional `dialect` config field (cleaner than
    base_url string inference). Low urgency; base_url inference works today.
  - ⬜ **M4 breadth**: Groq/xAI/Ollama = `StandardDialect` + presets (no known
    quirks). Add on demand.
- **Verdict:** the refactor achieved its goal (isolate the one real quirk, clean
  shared core). "Stalled" = near-complete, not blocked.

## 2. Effort B — CLI delegates (Doc 32) — **⬜ NOT STARTED (real work)**

Current state is worse than it looks (verified):

| CLI | spawn | sandbox |
|-----|-------|---------|
| claude_cli | own `SubprocessRunner` + tars-sandbox wrap (opt-in `TARS_CLAUDE_SANDBOX`) | tars-sandbox |
| gemini_cli | **its OWN** `SubprocessRunner` (`gemini_cli.rs:225`), bare `Command::new` | ⚠️ **NONE — unconfined black-box agent** |
| codex_cli | **third** private spawn (`codex_cli.rs:253-342`) | codex's own `--sandbox SAFE` flag (not tars) |

→ `SubprocessRunner`/spawn **copied 3×**, sandbox in 3 states, **gemini_cli is an
unsandboxed hole** (contradicts Doc 29).

- **M0** ✅ `CliDialect` trait + `AgentCliBackend` (`backends/cli/`) + `ClaudeCliDialect`.
  Lifted `SubprocessRunner`/sandbox-wrap/streaming/argv OUT of claude_cli (old
  `claude_cli/{argv,subprocess,streaming}.rs` DELETED — no dup); public API + registry
  unchanged (re-exports; `ClaudeCliProvider` = alias of `AgentCliBackend`). Behavior
  byte-for-byte (argv identity + event tests; `security_delegate_cli` green). 266
  tests. Documented M0 debt: sandbox wrap still inside `RealSubprocessRunner` (M4/G10
  hoists), runner builds argv internally (M1 makes `dialect.argv` load-bearing),
  `OutputMode::Text`+`env()` declared (wired M3).
- **M1** ✅ `GeminiCliDialect` + `CodexCliDialect` on `AgentCliBackend`; gemini's &
  codex's private spawns DELETED. Shared `build_sandboxed_command()` (`cli/subprocess.rs`)
  → **gemini now gets the same tars-sandbox write-jail as claude (the unconfined hole
  is CLOSED)**; codex keeps its own `--sandbox` + tars-sandbox on top (defense-in-depth).
  278 tests, `security_delegate_cli` green (claude byte-for-byte).
  - ✅ **RESOLVED (M4 caps-honesty):** codex caps now declare `streaming:false`/
    `supports_cancel:false` — honest for the buffered emit-after-turn runner. (Event
    content stays byte-for-byte; only the caps became truthful.) codex still honors
    `max_output_tokens`; gemini truncation flips EndTurn→MaxTokens (consistency).
- **M2** ✅ `OpenCodeDialect` (`opencode run --format json --model provider/model`,
  JsonEvents). Event schema grounded from opencode source (`run.ts` emit + `message-v2.ts`):
  `text`→Delta, `reasoning`→ThinkingDelta, `step_finish`→usage, `error`→raw-carrying.
  ✅ **Prior inline gaps now PINNED against opencode source (M4):** JSON mode has NO
  terminal event (`emit()` only fires text/reasoning/step_*/tool_use/error; the read loop
  `break`s on the internal `session.status:idle` without emitting it, and `message.updated`
  is gated `!== "json"`) → synthesizing `Finished(EndTurn)` is the correct behavior, not a
  fallback. Per-step `step_finish` tokens are PER-STEP deltas (`processor.ts` "finish-step"
  assigns `tokens = getUsage(value.usage)`, no accumulator vs. sibling `cost +=`) → summing
  yields the exact turn total. Comments tightened to the confirmed facts.
- **M3** ✅ `AntigravityDialect` (`agy -p "…" --model X --dangerously-skip-permissions
  --add-dir {wt}`, **Text** output — wired the `OutputMode::Text` path in `AgentCliBackend`
  that M0 stubbed). `env()` passes `GEMINI_API_KEY`/`ANTIGRAVITY_API_KEY` through the
  sandbox strip. Both delegates spawn through the shared sandboxed command.
  **All 5 CLI dialects now go through `AgentCliBackend` + uniform tars-sandbox.** 360
  provider+config tests green.
- **M4** ✅ **caps-honesty + unified sandbox policy (G10).**
  - **Caps honesty (all 5 delegates):** every CLI delegate runs through `AgentCliBackend`,
    which drains the delegate's stdout to completion THEN emits ChatEvents — none stream
    incrementally, none cancel mid-turn. Audited: claude/gemini/opencode/antigravity already
    declared `streaming:false`/`supports_cancel:false` (honest); **codex** was the lone liar
    (`streaming:true`/`supports_cancel:true`) — now set false/false with a comment stating why.
    New regression test `registry::every_cli_delegate_advertises_buffered_caps` builds all 5
    and asserts buffered caps.
  - **Unified sandbox (G10):** the CLI delegate spawn now folds onto the real policy path.
    `RequestContext` carries a `sandbox: SandboxPolicy` (tars-types → tars-sandbox, a zero-dep
    leaf), threaded from `AgentContext.sandbox` (worker `drain_one_call` + agent
    `drive_llm_call`) — i.e. the resolved `[sandbox]` TOML + `--sandbox` flag reaches the spawn,
    same as tools. `SubprocessInvocation` carries the policy.
- **M5** ✅ **CLI delegates CONFINED BY DEFAULT (FR-3/NFR-2) + runner consolidation (C2).**
  - **Default-confine (security, the closed hole):** a CLI delegate is a black-box coding agent,
    so it is now OS-sandboxed **unconditionally** — `resolve_effective_policy`
    (`cli/subprocess.rs`) DOWNGRADES the default/explicit `DangerFullAccess` to a workspace-write
    jail rooted at the worktree cwd (else the process cwd), and honors an explicit
    `ReadOnly`/`WorkspaceWrite`. The jail root falls back to the process cwd when no worktree cwd
    is threaded (SAFE) and **fails closed** (refuses to spawn) only if even that is unresolvable —
    a delegate is never silently run unconfined. `build_sandboxed_command` always wraps (fail-closed
    on a wrap/unsupported-platform error). The legacy `TARS_CLAUDE_SANDBOX=1` env gate is **no
    longer read** (confinement is unconditional; setting it is a redundant no-op that still yields
    confinement). ⚠️ **Deviation from a strict reading of "`--sandbox danger-full-access` opts a
    delegate up to unconfined":** `SandboxPolicy` cannot distinguish an *explicit* danger choice
    from the default within scope (a distinguishing bit would ripple through tars-config/tars-sandbox,
    16 construction sites + `PartialEq`), so a delegate is jailed even under `danger-full-access` —
    the SAFE option the security mandate calls for. To let a delegate write beyond the worktree,
    widen `[sandbox] writable_roots`.
  - **G10 remainder closed:** `drive_llm_call` (SingleShot/Orchestrator) now threads
    `req_ctx.cwd = ctx.cwd.clone()` (worker `drain_one_call` already did), so the jail roots at the
    per-step worktree when one exists. `AgentContext.cwd` is still `None` on the `run_plan` path
    (no per-step worktree today) → the delegate confines to the process cwd there.
  - **E2E-4 shipped:** `tests/security_delegate_cli_default_confine.rs` — a NON-claude delegate
    (shared runner + `GeminiCliDialect`) driven through the **policy default-confine path** (plain
    `SandboxPolicy::default()`, NO env gate) is BLOCKED from writing outside the worktree and
    ALLOWED inside it. The env-gate `security_delegate_cli` (claude) stays green byte-for-byte.
  - **Runner consolidation (Doc 32 §9 as-built gap):** the 5 near-duplicate `SubprocessRunner`
    impls collapse to **2**. gemini/codex/opencode/antigravity now share ONE
    `SharedCliRunner` (`cli/subprocess.rs`) — a single spawn + prompt-channel + concurrent-drain
    skeleton parameterized by the dialect's declared `OutputFraming` (`SingleObject{strip_prefix}` /
    `JsonLinesArray` / `RawText`, the 4 legacy framings). `CliDialect::output_framing()` is the new
    declared seam; the shared runner reads `dialect.argv`/`prompt_channel`/`output_framing`, so a new
    buffered CLI = a `CliDialect` with **no bespoke runner** (FR-6 now true). **Remainder:** claude
    keeps its own `RealSubprocessRunner` — its `stream-json` NDJSON path (`streaming.rs`) + child-reaper
    / process-group teardown are genuinely different from the buffered delegates; folding it would risk
    its byte-for-byte streaming behavior. All 5 delegates' events unchanged; `cargo test`
    (`tars-provider`/`tars-runtime`/`tars-config`/`tars-cli`) green; `clippy` clean (the lone
    warning is in the out-of-scope gemini HTTP adapter WIP). Live `run -P claude_cli --prompt hi`
    still answers, now sandboxed. (`gemini_cli` fails on Google's `IneligibleTierError` — a
    pre-existing auth/tier deprecation the raw `gemini` binary hits identically, not a regression.)
  - **Not covered:** no live per-platform validation of `read-only` mode for a delegate.

## 3. Effort C — Bedrock (Doc 31) — **✅ M0+M1 built** (M2/M3 deferred)

- **M0** ✅ crate `tars-bedrock` (AWS SDK fetched: `aws-sdk-bedrockruntime 1.135`;
  feature-gated, absent from default graph). `Value↔Document` shim, `Converse`
  mapping (unified), keyless lazy SigV4 client (`OnceCell`). URL-image = honest
  `InvalidRequest`, SDK errors carry the service message (CLAUDE.md #1). 11 + 3
  (config) + provider feature-build tests green; default build green.
  - ✅ **Doc 31 §6 C4 correction (design was cyclic) — now reflected in the doc:** "a
    `bedrock` feature on `tars-provider` forwarding to `tars-bedrock`" + "`impl
    LlmProvider` inside `tars-bedrock`" ⇒ a `tars-provider ↔ tars-bedrock` Cargo cycle.
    Resolved: all AWS logic in leaf `tars-bedrock` (deps only `tars-types`); the ~90-line
    `impl LlmProvider` bridge lives in `tars-provider::backends::bedrock`. Doc 31 §4/§6
    updated to the as-built leaf.
- **M1** ✅ `ConverseStream` — real incremental streaming (`client::stream_response` + pure `StreamTranslator`): text/thinking deltas, local tool-use JSON accumulation, `MessageStop`+`Metadata`→`Finished`; typed stream-error classifiers carry the SDK message; bridge `stream()` now on ConverseStream (`complete()` stays unary); 19 bedrock tests green. **M2** ⬜ embeddings/image (deferred). **M3** ⬜ the
  `Auth`-signer generalization (Doc 29 IdentityProvider seam).
- Value: production/cloud path — keyless, IAM-authed, workload-identity signs on AWS.

## 4. Guardrails

1. **Behavior-driven seam, shared core untouched** (OCP) — both OpenAiDialect and
   CliDialect. New variant = new impl, never edit shared.
2. **Every CLI delegate is a black box → uniform tars-sandbox** (no exceptions;
   gemini's gap is the proof it matters).
3. **Failures carry the raw** (FR-5), typed, no sentinels (CLAUDE.md #1).
4. **Consumer CLIs = best-effort behind routing fallback**, never the load-bearing
   path; product = DeepSeek/Bedrock.
5. **Don't manage a delegate's internals** (its perms/MCP are the user's config);
   wrap its process.
6. **Grounded, not guessed** — agy's interface came from the installed `agy --help`,
   not the web.

## 5. Next actions (keep both moving)

- **A (openai-compat):** FR-5 (raw-carrying error) — small, do when convenient.
- **B (CLI):** M0 (extract `AgentCliBackend`, closes gemini sandbox gap) → M1 migrate
  → M2 opencode → M3 antigravity. **The real remaining work.**
- **C (Bedrock):** M0 crate + Converse. Separate/parallel (new crate, no conflict).
