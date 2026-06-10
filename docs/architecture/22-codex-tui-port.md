# Doc 22 — Codex TUI port + the TARS tool layer

> **Status**: design, 2026-06-10. Decision captured: fork Codex's
> Rust TUI as our frontend and drive it with the TARS runtime over a
> direct Rust API (not an HTTP/OpenAI shim). This doc designs the port
> and — the bigger half — assesses how much of Codex's **tool layer** we
> lift versus build. Companion: [Doc 20](./20-agent-abstraction.md) (the
> Agent contract), [Doc 21](./21-tars-agent-impl-notes.md) (the native
> agent + the two-`ToolRegistry` debt this doc pays down), [Doc 05](./05-tools-mcp-skills.md)
> (the tool/MCP/skill design, mostly unshipped).

## 1. Goal

Reuse Codex's polished, fast Ratatui TUI as the interactive shell, but
replace its **brain** (model client + agent loop) with TARS, so that:

- every model call runs through `tars-pipeline` (cache / retry / routing /
  budget / observability) over **any** TARS provider, not just OpenAI;
- users can run **their own TARS agents** (Doc 20: a `SkillSet` + tools +
  prompt you hand a `Task`) inside that TUI;
- TARS gains a **production-grade tool layer** by lifting Codex's
  model-agnostic mechanism crates instead of writing sandboxing from
  scratch.

Both codebases are Rust and **Apache-2.0** → vendoring is license-clean
(retain Codex's `LICENSE` + `NOTICE`, attribute in our `NOTICE`).

## 2. The two gaps

**Gap A — we have no interactive coding frontend.** TARS exposes `tars-cli`
(one-shot) and `tars-server` (REST). No multi-turn TUI with diff review,
approvals, streaming exec output.

**Gap B — the tool layer is thin and forked.** This is the one the team
flagged. Today:

- `tars-tools`: a `Tool` trait (`execute(args, ToolContext{cwd, cancel})`)
  + `ToolRegistry` + 5 builtins (`read_file` / `list_dir` / `write_file` /
  `edit_file` / `bash`). **No sandbox** (`bash` just spawns), no approval
  UX, no patch system, no MCP.
- `tars-runtime::session.rs`: a **second**, incompatible `Tool` trait
  (`call(args) -> JsonValue`, no `ToolContext`, no cwd) + its own
  `ToolRegistry`. This is the Doc 21 §2 fork.
- Permissions exist (Doc 21 §5) but `Ask` == `Deny` until there's a
  human-prompt channel — which is exactly what a TUI provides.

A coding TUI needs the strong version of all of this. So the port forces —
and funds — the unification Doc 21 left open.

## 3. Strategy: port the shell + the mechanism, replace the brain

> **⚠️ Reality check (2026-06-10 recon of `openai/codex@rust-v0.57.0`).** The
> clean two-seam picture below was written from docs; the source is heavier:
>
> - **The conversation seam is real and clean** ✅ — `CodexConversation`
>   (`core/src/codex_conversation.rs`) is exactly `submit(op: Op) -> Result<String>`
>   + `next_event() -> Result<Event>`. A TARS-backed conversation only has to
>   satisfy that pair, and the TUI drives it.
> - **But `codex-tui` is NOT cleanly separable** ✗ — it's **37.6k LOC / 90
>   files** and depends on `codex-core` for much more than protocol:
>   `config::Config` (16 uses), `ConversationManager`, `AuthManager`,
>   `get_platform_sandbox`, plus `login` / `ollama` / `file-search` /
>   `app-server-protocol`. You cannot vendor "tui + protocol" alone.
> - **The `protocol` crate isn't leaf** — it pulls `codex-git`,
>   `codex-utils-image`, `mcp-types`, `icu_*`, `ts-rs`, `schemars`, `strum`.
> - Workspace is **43 crates** (not ~80).
>
> Net: "port tui+protocol, replace core" understates it. Three realistic
> paths, see §3a — the choice changes whether **TARS or Codex is the brain**.

codex-rs is a 43-crate workspace. The *intended* seams (optimistic version):

```
  tui  ──(protocol: Op / EventMsg)──  core ──  client(OpenAI)
  └ PORT ──────────┘                  └ REPLACE w/ TARS ┘   └ DELETE ┘
  sandboxing · apply-patch · exec · mcp   ← LIFT (model-agnostic mechanism)
```

## 3a. Three realistic paths (post-recon)

| Path | What you vendor / build | Brain | Cost | End state |
|---|---|---|---|---|
| **A — swap core's client** | tui + core + protocol (+ util deps); replace only codex-core's model-call layer with one calling `tars-pipeline` | **Codex's** loop | Lowest to "runs"; you keep ~all of codex-rs as a fork | Codex TUI routed through TARS *providers* (cache/retry/routing). TARS Agent layer unused. |
| **B — reimpl the conversation surface** | tui + protocol; build `tars-codex-core` that implements `CodexConversation` (`submit`/`next_event`) over `tars-runtime::Session` + maps `ChatEvent`→`EventMsg`; provide compat `Config`/auth shims | **TARS's** | High — must satisfy the Config/ConversationManager/auth surface 37.6k LOC of tui assumes | Codex's polished TUI, TARS as the brain; users run TARS agents. The stated goal. |
| **C — bespoke TUI over TARS** | a small new ratatui frontend (no codex fork); drive `tars-runtime::Session` directly | **TARS's** | Medium build, but small surface you *own* (no 37.6k-LOC fork, no upstream divergence) | TARS-native coding TUI; less polished than Codex, fully ours. |

Recommendation hinges on intent: if the goal is "Codex UX, TARS brain, users
build TARS agents" (the thread's direction), **B or C** — and **C avoids
owning a 37.6k-LOC fork** that drifts from a weekly-moving upstream (§9). **A**
only makes sense if the goal is "keep Codex's agent, just use TARS providers".

## 3b. DECISION (2026-06-10): copy the view, own the controller (a C/B hybrid)

Chosen direction: **own the implementation (TARS is the brain, users build TARS
agents) but copy Codex's UI verbatim** — don't reinvent the look, don't fork
the whole agent. The recon makes this clean:

- **`codex-tui` splits ~69 / 31 along exactly the right line.** 62 of 90 files
  (**~19.6k LOC**) are **pure view — zero `codex_core`** (ratatui only): the
  input composer (`bottom_pane/chat_composer.rs` 3.5k, `textarea.rs`),
  markdown rendering (`markdown_render*.rs`, `wrapping.rs`, `text_formatting.rs`),
  the bottom pane (footer, list-selection), the render harness
  (`custom_terminal.rs`, `tui.rs`, `insert_history.rs`), `exec_cell/render.rs`.
  → **copy these ~verbatim** (Apache-2.0; keep NOTICE). This IS "Codex's look".
- The 28 core-coupled files (**~18k LOC**) are the **controller**:
  `chatwidget.rs` (event→cell driver), `app.rs`/`lib.rs` (wires
  `ConversationManager`/`Config`/`AuthManager`), `history_cell.rs` /
  `diff_render.rs` (keep the rendering, swap the `codex_core` data types for
  TARS ones), and OpenAI-only features (`onboarding/auth.rs`, `resume_picker.rs`,
  cloud/login) → **rewrite the few that matter, drop the rest.**

**Shape:** a new `tars-tui` crate = Codex's copied view layer + a thin
controller that drives `tars-runtime::Session` (map `ChatEvent` /
`ToolCallStart/End` / approval → the view's transcript cells; input box →
`Session::send`; the approval modal → an `ApprovalSink` from Doc 23). No
`codex-core`, no `ConversationManager`, no 37.6k-LOC fork — just the view +
a small TARS-native brain. The Doc 23 tool layer (gated dispatch + `ApprovalSink`
+ cwd) is exactly what this controller needs. Detailed M0 plan: separate.

- **Port** `tui/` + `protocol/`: the UI and the `Op`/`EventMsg` contract
  between TUI and core. The TUI never learns about TARS — it speaks
  protocol.
- **Replace** `core/`'s agent loop + `client` with a new
  **`tars-codex-core`** adapter: holds a `tars_runtime::Session`, turns
  inbound `Op` into `session.send_text(...)`, and maps `tars_types::ChatEvent`
  (`Delta` / `ThinkingDelta` / `ToolCallStart` / `ArgsDelta` / `End` /
  `Finished`) → the `EventMsg` the TUI renders.
- **Lift** the mechanism crates (§5) into TARS's tool layer.

Use `Session` (multi-turn, interactive, internal tool loop — `session.rs:487`)
as the backend, **not** `TarsAgent::run(task)` (one-shot, `{summary,
confidence}` final-turn contract). The Agent abstraction comes back at M4
as a *selector* over Sessions, not the per-turn driver.

## 4. Codex tool-layer inventory

What Codex's `core` actually routes through, by crate:

| Crate | What it is |
|---|---|
| `apply-patch` | The `apply_patch` patch format + applier (the model's primary edit channel) |
| `sandboxing` / `linux-sandbox` | OS sandbox: Seatbelt (macOS SBPL), Landlock+seccomp (Linux), RestrictedToken (Windows). `SandboxPolicy{writable_roots, network, flags}` |
| `exec` (UnifiedExecProcessManager) | Sandboxed subprocess lifecycle + streamed stdout/stderr |
| `execpolicy` | Command allow/deny rules |
| `codex-mcp` / `mcp-client` / `mcp-server` | MCP integration, both directions |
| `core` ToolRouter | Approval gate → sandbox select → spawn → on-deny **escalation** (request approval, widen policy, retry) |
| `prompts` / tool schemas | `shell` / `apply_patch` / `update_plan` / `view_image` tool definitions shown to the model |

## 5. Port assessment — how much we lift

Rated on **portability** (self-contained, model-agnostic Rust?) and **value**
(how much it closes Gap B / how painful to write ourselves):

| Component | Port? | Portability | Value | Notes |
|---|---|---|---|---|
| **`apply-patch`** | **Lift ~as-is** | ★★★★★ | ★★★★★ | Pure patch logic, zero model coupling. Becomes TARS's real edit tool (replaces the thin `edit_file`). |
| **`sandboxing` + `linux-sandbox`** | **Lift** | ★★★★☆ | ★★★★★ | The crown jewel. Cross-platform Seatbelt/Landlock is months to write; it depends on `SandboxPolicy`, not on OpenAI. Glue = thread a `SandboxPolicy` into our `ToolContext`. |
| **`exec` / process manager** | **Lift + adapt** | ★★★★☆ | ★★★★☆ | Sandboxed spawn + streamed output. Coupled to Codex event types → rewire onto `ChatEvent`/`ToolContext`. Upgrades our `bash` from "naked spawn" to "sandboxed, streaming". |
| **MCP crates** | **Lift + adapt** | ★★★★☆ | ★★★★☆ | TARS has **no** MCP (Doc 05 designed, unshipped). Codex's client gives us MCP tools for free; some coupling to its tool model. |
| **ToolRouter + approval/escalation** | **Port the *pattern*** | ★★★☆☆ | ★★★★☆ | Woven into Codex's session. Don't copy wholesale — re-implement the gate→sandbox→escalate flow on top of TARS `Permissions`. This is what finally makes `Ask` real (the TUI is the prompt channel). |
| **`execpolicy`** | **Lift later** | ★★★★☆ | ★★★☆☆ | Useful, secondary; can follow. |
| **Tool schemas / prompts** | **Borrow** | ★★★★★ | ★★★☆☆ | Reuse `shell`/`apply_patch` schemas as our `ToolSpec`s; keep our naming (Doc 05). |
| **`core` agent loop + `client`** | **Do NOT port** | — | — | This is precisely what TARS replaces (`Session` + `tars-pipeline`). |

**Bottom line on "how much":** the entire *mechanism* tier (sandbox,
apply-patch, exec, MCP) ports — it's model-agnostic, Apache-2.0, and is
exactly Gap B. The *orchestration* tier (router/approval) ports as a
pattern onto TARS `Permissions`. The brain we throw away. Net: we get a
production tool layer for the cost of integration glue, not invention.

## 6. Target TARS tool layer (what the port forces us to build)

The port can't sit on two competing `Tool` traits. We unify on **one**
(retiring the `session.rs` registry per Doc 21 §2/§5):

```rust
pub struct ToolContext {
    pub cwd: Option<PathBuf>,
    pub cancel: CancellationToken,
    pub sandbox: SandboxPolicy,      // NEW — lifted from Codex
    pub approval: ApprovalSink,      // NEW — async "ask the human", backed by the TUI
}

#[async_trait] pub trait Tool {
    fn name(&self) -> &str;
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}
```

- `ApprovalSink` is the seam that makes `Permissions::Ask` real: a tool (or
  the router) calls `ctx.approval.request(...)`, which in the TUI renders
  Codex's approval widget and awaits the keypress; in headless/`tars-cli`
  it maps to the existing Deny-or-config-allow.
- Builtins reframed: `bash` → sandboxed `exec`; `edit_file`/`write_file` →
  `apply_patch`; plus MCP tools registered dynamically.
- `Session` and the (future) cwd-aware agent loop both dispatch through
  **this** registry — closing the Doc 21 §2 fork.

## 7. The TUI seam

`tars-codex-core` implements the core side of the ported `protocol`:

```
TUI ──Op(UserInput / ApproveExec / Interrupt)──▶ tars-codex-core
                                                   │ Session.send_text / approval resolve
TUI ◀──EventMsg(AgentMessageDelta / ExecBegin /    │ map ChatEvent + tool events
        ExecOutput / PatchApply / ApprovalRequest /│
        TokenCount / TaskComplete)─────────────────┘
```

We implement a **subset** of `EventMsg` first — only what M1–M3 need
(text delta, exec begin/output/end, patch apply, approval request, done).
Reasoning items, web-search, image, and the richer MCP/automation events
come later or get stubbed.

## 8. Milestones

- **M0** — vendor codex `tui/` + `protocol/` (pinned tag, e.g. `rust-v0.57.0`)
  into `crates/`; build in-workspace against a stub core; TUI renders.
- **M1** — `tars-codex-core` text path: a turn → `Session` over `tars-pipeline`;
  streamed reply renders. **No tools.**
- **Tool-layer wave** (the Gap-B work, gates M2):
  - **T1** unify on one `Tool`/`ToolContext` (§6), retire the `session.rs`
    registry (Doc 21 §2).
  - **T2** lift `apply-patch`; lift `sandboxing`; rebuild `bash`→sandboxed `exec`.
  - **T3** approval/escalation onto `Permissions` + `ApprovalSink`.
- **M2** — one sandboxed tool (`exec`) + approval round-trips in the TUI.
- **M3** — `apply_patch` → the TUI diff/patch widgets; full builtin set.
- **M4** — agent selection: pick/define a TARS agent (`SkillSet` + provider +
  prompt) the TUI switches between. **The "users build their own apps" payoff.**
- **M5** — prune Codex-isms (OpenAI assumptions, branding, replaced crates);
  optionally lift MCP + `execpolicy`.

## 9. Risks / open decisions

- **Fork divergence.** codex-rs moves weekly. Vendor a **pinned snapshot**
  and treat it as ours — do not chase upstream. Accept the maintenance.
- **Protocol breadth.** `EventMsg`/`Op` is rich (exec, MCP, reasoning,
  approvals, diffs, automations). Ship a subset; stub the rest loudly.
- **Sandbox lift depth.** The macOS/Linux sandbox crates may pull Codex
  internal deps; budget real time to excise those vs. the clean
  `SandboxPolicy` boundary. This is the highest-risk lift and the highest
  value — sequence it deliberately (T2), not opportunistically.
- **`Ask` semantics.** Wiring `ApprovalSink` finally makes `Ask` ≠ `Deny`
  (Doc 21 §5); confirm the headless fallback stays fail-closed.
- **Scope honesty.** This is two projects stapled together (a TUI port AND
  a tool-layer rebuild). The tool layer is arguably the larger, more
  durable win — it stands on its own even if the TUI port stalls.
