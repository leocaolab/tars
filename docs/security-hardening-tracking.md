# Security Hardening — Tracking Doc

Living checklist for the agent-security / exec-isolation effort. Use it to keep
future work on track. Companion to the **designs**: [Doc 22 §4-5](architecture/22-codex-tui-port.md)
(sandbox lift plan — the crown jewel), [Doc 29](architecture/29-agent-security.md)
(authz + isolation + cloud), [Doc 10](architecture/10-security-model.md) (threat
model), [Doc 05](architecture/05-tools-mcp-skills.md) (tools).

Status legend: ✅ done · 🚧 in progress · ⬜ not started · ⚠️ gap/risk.

---

## 1. Goal

arc is releasing. Its agents must **not exceed their permissions on the user's
machine** — must not see/touch/damage anything beyond the repo they work on.
Two concrete agents:

- **fixer / merge** — delegates to the **`claude` CLI** (`--permission-mode
  bypassPermissions`, so claude's own confinement is OFF). Needs broad writes to
  *merge* — but only within the worktree.
- **reviewer** — uses **deepseek** (HTTP inference). Read-only today; **will get
  `bash` (and git) in the future** — must stay confined then too.

Primary target **local** (macOS + Linux dev); **big goal is server** (autonomous,
so **no interactive approval possible**).

## 2. Decision log (with rationale — do not re-litigate)

| # | Decision | Why |
|---|----------|-----|
| D1 | Confinement = **codex write-jail** (read broad, write only worktree) + network toggle | A deny-default *read*-jail aborts the process on macOS Seatbelt (validated on-box); codex's `WorkspaceWrite` is the proven model. "和codex一致." |
| D2 | **No interactive approval on server** → policy-based authz (deny-by-default, **explicit-deny-wins**, AWS/BeyondProd), never a prompt | Server is autonomous; can't prompt. `Ask` == `Deny` on server. |
| D3 | **`rm -rf` defense = OS filesystem jail on exec**, NOT a command denylist | Denylists leak (infinite spellings); fs scope is structural. |
| D4 | Sandbox home = **new `tars-sandbox` mechanism crate**, below `tars-tools` + `tars-provider` (siblings on `tars-types`) | `tars-provider` is the LLM layer — wrong place. Fills the Doc 22 T2 seam. Both BashTool (tools) and claude_cli (provider) need it. |
| D5 | `SandboxMode` = **ReadOnly \| WorkspaceWrite \| DangerFullAccess** (codex 3 modes); policy is **per-agent/role** via `ToolContext.sandbox` | fixer=WorkspaceWrite, reviewer=ReadOnly (→ WorkspaceWrite when it gets write-git). |
| D6 | Config surface = **`[sandbox]` TOML + `--sandbox <mode>` flag** (codex-consistent), threaded into `ToolContext.sandbox` | User security config must flow in from TOML or `--` flag, per role. |
| D7 | **Don't manage claude's internals** (black box) — the OS box is the only boundary | User configures claude on the claude side; we wrap the *process*. |
| D8 | Test with **real agents** (mock providers + real tools), not hand-synthesized scripts | Two mocks: MockLlmProvider (native-tool path) + MockCliProvider (delegate path). |
| D9 | Cloud (future) = **inherit** identity/secrets/sandbox from AWS/GCP via seams; tars owns only **tenant/agent authz** | Don't reinvent IRSA/gVisor/Secrets Manager. Portable via seams. |

## 3. Architecture — two paths, two guards, two mocks

| Path | provider | tools used | guard | test via |
|------|----------|-----------|-------|----------|
| **fixer/merge** | `claude_cli` (subprocess, black box) | claude's OWN | **OS sandbox** (`tars-sandbox`) around the process | **MockCliProvider** + mock CLI |
| **reviewer / native** | deepseek / mock (inference) | **tars's own** (read/write/bash/git/web) | `ctx.sandbox` in-process jail (+ OS sandbox for bash) | **MockLlmProvider** + real tools |

Maps to the earlier rule: file-jail is *enough* for the native-tool path (was
already ~there via `with_root`), but **not** for the black-box subprocess → OS
sandbox. deepseek+bash(future) = ReadOnly mode now, WorkspaceWrite/scoped later.

## 4. Gaps found (from reading the code)

| ⚠️ | Gap | Location |
|----|-----|----------|
| G1 | `tars_tools::SandboxPolicy` is a **stub**, default **unrestricted**, **enforced by nobody** | `tars-tools/src/sandbox.rs` |
| G2 | **BashTool = naked `Command::new("sh")`** — no jail, no sandbox, no roots | `tars-tools/src/builtins/bash.rs:91` |
| G3 | Confinement is **uneven & opt-in**: Read/Write use per-tool `with_root`; Glob/Grep use `ctx.readable_roots`; Bash uses nothing; `ctx.sandbox` unused | tars-tools builtins |
| G4 | **WriteFile TOCTOU**: checks canonical ancestor, writes non-canonical `combined` → symlink swap window | `write_file.rs` resolve |
| G5 | User security config **not threaded** into `ToolContext` | config → ToolContext |
| G6 | No **git** tool, no **web** tool (only read/write/edit/bash/grep/glob/list_dir) | tars-tools builtins |
| G7 | **MCP** not implemented; **Skills** are declarative tags only (not executable) | Doc 05 (design), TODO B-9 (deferred) |
| G8 | I initially put the sandbox in **tars-provider** (wrong layer) — moved to `tars-sandbox` ✅ | — |
| G9 | ✅ **RESOLVED (M4)**: `AgentContext` now carries a `SandboxPolicy`; worker.rs threads it (via `resolve_step_sandbox`) into the loop's `ToolContext.sandbox`. WorkerAgent (arc's driver) now enforces the configured sandbox — a runtime test asserts WorkspaceWrite+`writable_roots==[cwd]` reaches the `ToolContext`. | `crates/tars-runtime/src/worker.rs` |
| G10 | ⚠️ **arc-critical inconsistency**: the **claude_cli delegate** (arc's fixer/merge) sandbox is gated by a SEPARATE `TARS_CLAUDE_SANDBOX=1` **env var** + hardcoded `workspace_write(cwd)` — it does NOT read `[sandbox]`/`--sandbox`. So `tars --sandbox workspace-write` confines the tools but NOT the claude_cli delegate. **Unify**: thread the resolved `SandboxPolicy` through `RequestContext` into the provider so claude_cli reads the same config (and honours ReadOnly/mode). Until then the delegate needs the env var explicitly. | `crates/tars-provider/src/backends/claude_cli/subprocess.rs:21` |

## 5. Milestones / status

- **M0 — `tars-sandbox` crate** ✅ created + compiling. `SandboxPolicy{mode,
  writable_roots, network}` + `SandboxMode` (3) + `wrap()` (Seatbelt/bwrap) +
  `SandboxError`. 5 unit tests. **macOS write-jail VALIDATED on-box**: writes/`rm`
  to `$HOME` + `/etc` → `Operation not permitted`; `~/.zshrc` survives `rm`; the
  integration test **caught + fixed a `/tmp`-writable hole**.
- **M1 — wire consumers, delete the dup** ✅
  - ✅ `tars-tools` deps `tars-sandbox`; `SandboxPolicy` re-exported; imports fixed;
    stub deleted. Builds green.
  - ✅ `tars-provider` deps `tars-sandbox`; claude_cli uses
    `SandboxPolicy::workspace_write` (maps `SandboxError`→`ProviderError`); the
    misplaced dup `tars-provider/src/sandbox.rs` **deleted** + module reg removed;
    `sandbox_jail.rs` moved to `tars-sandbox/tests/` (5 unit + 1 jail green; 19
    claude_cli green).
- **M2 — BashTool enforces `ctx.sandbox`** ✅ (G2) — `sh -c` wrapped via
  `ctx.sandbox.wrap` ("naked spawn → sandboxed", Doc 22). Default `DangerFullAccess`
  = passthrough (behaviour unchanged until M4 threads a policy). **Proven on macOS**:
  with a `WorkspaceWrite` policy, bash write inside worktree succeeds, write outside
  is blocked (`tars-tools/src/builtins/bash.rs` test). 65 tars-tools tests green.
- **M3 — unify fs-tool enforcement** ✅ (G3, G4) — WriteFile/ReadFile now enforce
  `ctx.sandbox` AND-ed with `with_root` (defense in depth): `WorkspaceWrite`→under
  canonical `writable_roots`, `ReadOnly`→deny all writes, `DangerFullAccess`→
  unchanged, empty-roots→fail-closed. **TOCTOU closed** (re-canonicalize the real
  parent after `create_dir_all`, re-verify, write `real_parent.join(basename)`).
  6 new tests incl. `toctou_symlinked_parent_swap_cannot_escape`. 69 green.
- **M4 — config → policy → ToolContext** ✅ (G5, D5, D6) — `[sandbox]` TOML
  (`tars-config/src/sandbox.rs`, kebab modes, flag-over-TOML `resolve_policy`) +
  global `--sandbox <mode>` flag. Threaded end-to-end: flag/TOML → RunTaskConfig →
  RunPlanConfig → **WorkerContext/CriticContext (per-role seam)** → execute_agent_step
  → `AgentContext.sandbox` → **`ToolContext.sandbox` at the tool-dispatch site
  (worker.rs)** — this **resolves G9** (WorkerAgent now carries the policy).
  `WorkspaceWrite` w/o explicit roots → scoped to `ctx.cwd`. Default preserved
  (absent ⇒ DangerFullAccess). 8 config + 2 runtime threading tests.
- **M5 — mock test infra** ✅ — existing `MockProvider` sufficed as MockLlmProvider
  (emits `ToolCallStart/End`); a mock "claude" CLI script drives the delegate path.
- **M6 — security integration tests** ✅ — both paths, real loop + real sandbox,
  macOS-gated, offline:
  - `crates/tars-runtime/tests/security_native_agent.rs` — MockLlm → real Session
    tool loop → BashTool → Seatbelt: escape blocked, inside write OK.
  - `crates/tars-provider/tests/security_delegate_cli.rs` — mock CLI → **real
    `RealSubprocessRunner`** → Seatbelt: outside create blocked, victim survives
    `rm`, JSON round-trips; **non-vacuity proven** (flag OFF → escape assertion fails).
- **M7 — later** ⬜ — git + web tools; egress allowlist (network proxy);
  `AuthzPolicy` PDP + IAM middleware (Doc 29); cloud seams (IRSA/gVisor/Secrets);
  make `Ask` real (approval channel) for non-server contexts.

## 6. Guardrails (check every change against these)

1. **Sandbox lives in `tars-sandbox`** (mechanism), threaded via `ToolContext` —
   **never** in the LLM/provider layer.
2. **Fail-closed** everywhere: sandbox can't build → refuse to spawn; missing
   policy → deny; no `unwrap_or(Allow)`, no raw-`sh` fallback.
3. **Deny-by-default** on writes outside the worktree; server has **no `Ask`**.
4. **Structural defense** (fs jail), never command denylists.
5. **Black box = external box**: don't manage claude's internals; jail its process.
6. **Real-agent tests** (mock provider + tools), offline (cassette/mock) in CI —
   two paths, two guards, two mocks.
7. **Portable**: zero AWS/GCP symbols in core; cloud behind seams.
8. **Validate exec sandboxes on-box** before flipping a default on (macOS ✅;
   Linux bwrap + live `claude -p` still ⬜).

## 7. Open validation debts

- ⬜ Linux `bwrap` write-jail — validate on a Linux box (mac can't run bwrap).
- ⬜ Live `claude -p` inside the jail: auth via readable `~/.claude`, API reachable,
   worktree+TMPDIR writable, actually confined — **watch claude/node install path
   under `$HOME`** (nvm/npm-global) not being readable would break it.
- ⬜ git needs some `.git` writes even for "read" ops (index.lock) — scope when
   the reviewer gets git.

### Housekeeping (pre-existing, NOT from this effort)
- ⬜ `crates/tars-runtime/examples/non_llm_plan.rs` fails to compile
  (`BlackboardStore::append_event` called with 7 args, trait declares 8 — a stale
  example after a concurrent trait change). Blocks `cargo test -p tars-runtime`
  (which builds examples); `--lib --tests` is green. 1-line fix, unrelated to
  sandboxing.
- ⬜ 2 pre-existing clippy warnings outside this effort: `tars-types/src/lib.rs:20`
  (decode-seam doc overindent), `tars-storage/blackboard/mod.rs:160`
  (`append_event` 8/7 args).

---

## 8. Capability landscape — tars vs peers (from the research)

How the field builds agents, and where tars stands. (✅ has · ⚠️ partial · ❌ none)

| Capability | codex | claude-code | opencode | rig | **tars** |
|---|---|---|---|---|---|
| Built-in tools | shell/apply_patch/list_dir/grep/web_search/**CSV fan-out**/multi-agent | Read/Write/Edit/Bash/Grep/Glob/**WebFetch/Search**/Task/Skill | file/shell/LSP/web | (library — you bring tools) | read/write/edit/bash/grep/glob/list_dir |
| **MCP** (external tool servers) | ✅ client+server | ✅ stdio/http/sse/ws, `mcp__server__tool`, OAuth | ✅ | — | ❌ **design only** (Doc 05; TODO B-9 deferred) |
| **Skills** (executable) | ❌ (TOML roles) | ✅ **SKILL.md + progressive disclosure** | ✅ | — | ❌ **declarative name-tags only** (`SkillSet`) |
| **RAG / vector / embeddings** | ❌ | ❌ (agentic search) | ❌ | ✅✅ **first-class** | ❌ |
| **Web** browse/fetch | provider-side `web_search` | ✅ WebFetch/Search | ✅ | — | ❌ (`web.fetch` planned name only; via `bash` curl) |
| **git** tool | via shell/apply_patch | via Bash | — | — | ❌ (via `bash` only; `git.*` are example names) |
| **DB / SQL / CSV / dataframe** | CSV fan-out only | ❌ | ❌ | — | ❌ (via `bash`; tars's own rusqlite is infra, not an agent tool) |
| **exec sandbox** | ✅ Seatbelt/bwrap+seccomp/Win + `execpolicy` | ✅ `@anthropic-ai/sandbox-runtime` (Seatbelt/bwrap) | partial | — (library) | 🚧 `tars-sandbox` (this effort) |
| **approval** | ✅ `approval_policy` + interactive | ✅ 4 modes + hooks + rules | ✅ interactive | — | ⚠️ `Permissions` types exist; `Ask` fails-closed==Deny (no channel) |

**Consensus from the field:** the three real agent products (codex/claude-code/
opencode) all treat **sandbox + interactive-approval + MCP** as core; **none use
RAG/embeddings** (all use agentic grep/glob search). rig is a *library* (RAG-first,
no imposed security). tars is thinnest on **exec-confinement + approval + MCP** —
exactly this hardening effort.

## 9. tars capability gaps (backlog, beyond security)

| Gap | State | Path today |
|-----|-------|-----------|
| MCP | design (Doc 05), deferred (TODO B-9) | none — the general answer to "stop writing a builtin per need" |
| Executable Skills (composite) | design (Doc 05 §1) | only declarative `Skill{name,description}` |
| git tool | not built | `bash` (git push gated on Saga/Backtrack per TODO) |
| web fetch/search tool | not built (planned name) | `bash` curl |
| DB / CSV / dataframe / Polars | none | `bash` (python/duckdb/jq) or custom `Tool` |
| RAG / embeddings | none | agentic grep/glob (like codex/claude-code) |
| Escape hatch for all | ✅ | generic `bash` + custom `Tool` trait |

## 10. Authz / cloud design (captured — full in Doc 29)

**config → policy → auth, as middleware** (the field pattern):
- **Envoy/gRPC**: policy = RBAC config / ext_authz PDP; enforced by a
  filter/interceptor (PEP). **AWS IAM**: deny-by-default, **explicit-deny-wins**,
  JSON policy. **GCP IAM**: role bindings, config-as-code.
- **tars already has the slot**: Doc 02 reserves **L2 "Auth & IAM, before Cache,
  cannot bypass"**; cache-key already folds IAM scopes (`tars-cache/src/key.rs:44`);
  `Principal`/`Scope` exist (`tars-types/src/principal.rs`, shape open for
  RBAC/ABAC/OPA). **Missing**: the IAM middleware is unbuilt; agent-side
  `Permissions` (default-permissive, Ask==Deny) not unified with `Principal/Scope`.
- **Server security (Google BeyondProd model)**: workload identity (ALTS) +
  non-interactive policy authz + **gVisor** sandbox for untrusted exec. Agent binds
  `Principal` + policy at start; every tool/RPC checked, deny-by-default.
- **Cloud integration (inherit, don't reinvent)**:
  - **AWS**: IRSA/Fargate identity · Bedrock (SigV4) · Secrets Manager · Firecracker/
    Fargate isolate · VPC endpoints · CloudTrail.
  - **GCP**: Workload Identity SA · Vertex (ADC) · Secret Manager · **GKE Sandbox =
    gVisor runtimeClass** (cheapest) · VPC-SC · Cloud Audit Logs.
  - **Local↔cloud bridge**: SSO/ADC short-lived creds (no keys on disk), cloud-vault
    secrets, local bwrap/Seatbelt, audit to cloud sink.
- **Seams (portability)**: `AuthzPolicy` (PDP) · `IdentityProvider` · `SecretProvider`
  · `ExecSandbox` (=`tars-sandbox`) · `AuditSink`. Core stays cloud-agnostic; tars
  owns only **tenant↔tenant / agent↔tool** authz (the layer the cloud can't do).

## 11. Sources (competitive research, 2026-07)

Snapshot/approval + eval: [insta](https://insta.rs/docs/cli/) ·
[ApprovalTests](https://github.com/approvals/ApprovalTests.Python) ·
[promptfoo](https://www.promptfoo.dev/docs/configuration/expected-outputs/) ·
[LangSmith](https://docs.langchain.com/langsmith/evaluation-concepts) ·
[vcr-langchain](https://github.com/amosjyng/vcr-langchain).
Agents: local `codex` (`/Users/hucao/projects/codex`), `claude-code`
(`/Users/hucao/projects/claude-code`), `opencode` (`/Users/hucao/projects/opencode`);
[rig](https://github.com/0xPlaygrounds/rig).
Cloud/security: [Google BeyondProd](https://docs.cloud.google.com/docs/security/beyondprod) ·
[gVisor](https://gvisor.dev/) ·
[AWS IAM eval](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_policies_evaluation-logic.html) ·
[Envoy ext_authz](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/ext_authz_filter).

## 12. Related prior work this session (already shipped / documented)

- **Decode seam** (`tars-types::json_decode`) — result-side typed decode. `v0.8.0`.
  USER-GUIDE + CHANGELOG.
- **Bless store** (`tars-types::bless`) — field-level golden assertions over a
  cassette-pinned reply. `v0.9.0`. [Doc 28](architecture/28-bless-store.md),
  py/ts bindings, cassette tests. (Separate from this security effort.)

