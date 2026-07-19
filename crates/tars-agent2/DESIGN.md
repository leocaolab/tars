# tars-agent2 — the coalgebra agent framework

## 0. Overview & goal

`tars-agent2` is a second, independent agent framework built on one thesis (grounded in
`tars-internal/docs/architecture/agent/` docs 11, 13, 14, 15):

> **The agent is a coalgebra — one process, a single decide-emit step function, holding no
> state.** All state lives in the **world** (a set of versioned components). The **god-program**
> is the algebra (the Runtime): it renders the world into a scoped view, folds the agent's
> emitted intents into effects, and drives the world to a **fixed point** — the point where the
> **Diff (desired − actual) is empty** — or runs out of fuel.

The load-bearing constraint, measured in doc 14 §3.7: the loop reaches a fixed point **only to
the degree the Diff is a cheap deterministic check** (build / test / lint / render oracle). A
Diff built from a noisy LLM re-judgment re-flags inputs it just passed and the loop *oscillates*
with no reachable fixed point. The arc v11 world-model demo drove its loop on exactly such a
noisy check and, honestly, never reached 0 open — it only *bounded* the oscillation via
memoization. `tars-agent2` therefore puts the determinism in the `Check` (a real `cargo test`),
where a genuine fixed point exists.

Non-goal / boundary: this crate does **not** touch `tars-agent` (the existing domain-model
crate). It reuses `tars-pipeline` (the LLM callable), `tars-types` (request/event shapes), and
`tars-provider` (the provider + mock). The LLM-driven agent calls the model **only** through
`tars_pipeline::LlmService` — never raw HTTP.

---

## 1. CUJs (concrete real journeys, real commands)

### CUJ-1 — "make a red `cargo test` green" (the deterministic-Diff exemplar)

- **Actor:** a developer (or a CI bot) with a repo whose test suite is red.
- **Trigger:** `cargo test` exits non-zero.
- **Desired state (the Spec):** one `Check` = `ShellCheck("cargo test", cwd=repo)`.
- **Steps (real commands):**
  1. Runtime computes the Diff: runs `cargo test` in the repo → exit 101 → `Red { detail: <the
     compiler/test output> }`. Gap = `[cargo-test]`. Not converged.
  2. Runtime renders the world (the source `File` components + the red gap) into a `View` and
     hands it to the agent.
  3. `LlmAgent` builds a `ChatRequest` (system + the rendered view + a `write`/`edit` tool),
     calls `LlmService::call`, drains the stream, turns the model's tool call into an
     `Intent { component: "src/lib.rs", handler: "write", args: {"content": "<fixed source>"} }`.
  4. Runtime applies the intent: the `File` handler writes the file **to disk** and bumps its
     content-hash version (`onUpdate`). Observation = `Applied { new_version, render }`.
  5. Runtime re-computes the Diff: `cargo test` now exits 0 → `Green`. Gap empty.
- **Success:** `world.converged(spec)` is true — the loop returns `Outcome::Converged`. The
  fixed point is real because it is `cargo test`'s exit code, not an opinion.

This is the CUJ the reference world (`components::{File, ShellCheck}`) and `tests/reconcile.rs`
implement mechanically (with a fast `grep` check standing in for `cargo test` so the test is
hermetic; the `ShellCheck` API runs any command — pass `("cargo", ["test"])` for the real
journey). `tests/llm_agent.rs` runs the same loop with an `LlmAgent` over a mock provider.

### CUJ-2 — "review a repo → record findings → fix → verify → reconcile to 0 open"

This is the arc `observe-operate-review` v11 journey
(`/Users/hucao/projects/arc/demos/observe-operate-review/v11-world-model/`).

- **Actor:** a reviewer agent over a fixed corpus of Rust files with planted panics.
- **Trigger:** `cargo run` (the harness), a set of files never yet reviewed.
- **Steps:** each `File` is a versioned component; a memoized `review` derives its findings keyed
  on the file's content version (`seen@version` / `dirty` marks). "Open findings" is a **derived
  view** over `cache[(file, current_version)]`, not a stored status table. The fixer emits
  `write` intents; a write bumps the version → invalidates `seen` → only changed files are
  re-reviewed. Reconcile = drive derived-open toward 0.
- **Success:** `derived-open == 0` (fixed point) OR `MAX_ROUNDS` fuel.
- **The honest caveat (the whole point):** v11's "verify" is an **LLM re-review** — a *noisy*
  Diff. Measured: it never reached 0 (ended at 2 open on false positives); it only *bounded* the
  oscillation `10→3→4→3→3→2` because memoization stopped re-sampling clean+seen files. This CUJ
  is the counter-example that proves CUJ-1's constraint: **a noisy Diff has no fixed point**;
  memoization (`seen@version` as the memo key) is what keeps it bounded. The framework must
  therefore make the `Check` deterministic where it can, and treat a noisy check as a
  fuel-bounded best-effort, never a convergence guarantee.

The mapping onto this crate: `File` (versioned, `onUpdate` on write) = the demo's
`FileComponent`; findings-as-derived-memoized-view = a future `Check` whose `eval` memoizes on
`Component::version`; the `Runtime.anneal` loop = the demo's `loop { ensure_reviewed; if open==0
break; run_fixer }`. The demo is the porting target; this crate is the reusable framework under
it.

---

## 2. Features → Requirements

| Feature | Requirement |
|---|---|
| F1 Stateless coalgebra agent | `Agent::step(&View) -> Step` holds no world state; safe to drop/replay. `Step = Emit(Vec<Intent>) \| ProposeHalt \| Park(Wake)`. |
| F2 World-of-components | `Component = render + handlers + handle(onUpdate: version bump)`; `World` owns all state; `apply(intent) -> Observation`. |
| F3 Deterministic Diff | `Spec` = set of `Check`; `gap(world)` = red checks; `gap.is_empty()` = fixed point. `ShellCheck` = real-command oracle. |
| F4 God-program reconcile loop | `Runtime.anneal(world, spec, agent)` → `Converged \| Exhausted{gap} \| Parked`. Termination = fixed point OR fuel, **verified against the world**, never the agent's self-report. |
| F5 Source-truth observations | On a handler failure, the `Observation` carries the **raw args + real error** — never a sentinel token. |
| F6 LLM agent on the pipeline | `LlmAgent` decides via `LlmService` (built with `LlmService::builder`), parses tool calls → `Intent`. No raw HTTP. |

---

## 3. Components + reuse map (every reuse cites real file:line)

### Own components (`crates/tars-agent2/src/`)

- `agent.rs` — `Agent` trait (`async fn step`), `Intent`, `Step`, `Wake`.
- `world.rs` — `Component` trait, `World` (`apply`, `converged`), `CompId`, `Version`.
- `diff.rs` — `Check`, `CheckResult`, `Spec`, `Gap`, `RedCheck`.
- `render.rs` — `View`, `CompView`, `View::to_prompt`.
- `effect.rs` — `Observation` (`Applied` / `Failed`).
- `runtime.rs` — `Runtime`, `Outcome`, the `anneal` reconcile loop.
- `components.rs` — reference world: `File` (versioned, disk-backed), `ShellCheck` (deterministic Diff).
- `llm.rs` — `LlmAgent` (the pipeline-driven decider).

### Reuse map — exact symbols called

| Need | Symbol | Real location |
|---|---|---|
| Build the LLM callable | `LlmService::builder(provider, model) -> LlmServiceBuilder` | `crates/tars-pipeline/src/middleware.rs:61` |
| Add middleware / finish | `LlmServiceBuilder::layer` / `::build` | `crates/tars-pipeline/src/middleware.rs:240`, `:247` |
| Canonical onion (prod) | `LlmService::default_chain(provider, model, ChainOpts)` | `crates/tars-pipeline/src/middleware.rs:99` |
| Issue the call | `LlmService::call(req, ctx) -> Result<LlmEventStream, ProviderError>` | `crates/tars-pipeline/src/service.rs:62` |
| The streamed events | `LlmEventStream` (`Stream<Item = Result<ChatEvent, _>>`) | re-export `crates/tars-pipeline/src/lib.rs:114` (from `tars_provider::provider`) |
| Request shape | `ChatRequest` / `::user` / `::with_system` | `crates/tars-types/src/chat.rs:20`, `:75`, `:95` |
| Tool definition sent to model | `ToolSpec` / `ToolSpec::new` | `crates/tars-types/src/tools.rs:19`, `:37` |
| Tool-args schema | `JsonSchema::loose` | `crates/tars-types/src/schema.rs` (`pub use` `lib.rs:80`) |
| Tool selection | `ToolChoice::Auto` | `crates/tars-types/src/tools.rs:64` |
| Model's tool call (correlated by index) | `ChatEvent::ToolCallStart{index,name}` / `ToolCallEnd{index,parsed_args}` | `crates/tars-types/src/events.rs:30`, `:41` |
| Turn end / reason | `ChatEvent::Finished` / `StopReason` | `crates/tars-types/src/events.rs:55`, `:64` |
| Per-call context | `RequestContext::test_default` / `::personal` / `::with_cwd` | `crates/tars-types/src/context.rs:110`, `:91`, `:130` |
| Hermetic provider (tests) | `MockProvider::with_responses` / `CannedResponse::Sequence` | `crates/tars-provider/src/backends/mock.rs` (`pub use` `tars_provider::lib.rs:64`) |

**Relation to `tars-agent` (untouched):** `tars-agent` defines a *different* `Agent`
(`async fn run(Task) -> AgentOutput`, `crates/tars-agent/src/lib.rs`) — a task-executing domain
agent. `tars-agent2`'s `Agent` is the coalgebra step function. They are deliberately distinct
abstractions; agent-2 does not depend on or modify `tars-agent`.

---

## 4. Interfaces (the pinned shapes)

```rust
// agent.rs — the coalgebra
#[async_trait] trait Agent { async fn step(&mut self, view: &View) -> Step; }
enum Step { Emit(Vec<Intent>), ProposeHalt, Park(Wake) }
struct Intent { component: CompId, handler: String, args: String }

// world.rs — where all state lives
trait Component {
    fn id(&self) -> CompId;
    fn version(&self) -> Version;         // content-hash: identity + memo key + CAS token
    fn render(&self) -> String;
    fn handlers(&self) -> Vec<String>;
    fn handle(&mut self, handler: &str, args: &str) -> Observation;  // onUpdate: bump version
}
impl World { fn apply(&mut self, i: &Intent) -> Observation; fn converged(&self, s: &Spec) -> bool; }

// diff.rs — desired − actual
trait Check { fn id(&self) -> String; fn eval(&self, w: &World) -> CheckResult; }
enum CheckResult { Green, Red { detail: String } }
impl Spec { fn gap(&self, w: &World) -> Gap; }          // gap.is_empty() = fixed point

// runtime.rs — the god-program
impl Runtime { async fn anneal(&self, w: &mut World, s: &Spec, a: &mut dyn Agent) -> Outcome; }
enum Outcome { Converged{iters}, Exhausted{iters, gap}, Parked{iters, wake} }
```

The reconcile loop (`Runtime::anneal`): `while fuel: if converged → Converged; render →
agent.step → { Emit → apply each (Push effect, record Observation); ProposeHalt → verify gap,
accept only if truly empty else Exhausted; Park → suspend } `.

---

## 5. E2E tests (real, passing)

- `tests/reconcile.rs::reconcile_reaches_fixed_point_on_deterministic_check` — the CUJ-1 loop:
  a `ShellCheck` (real `sh -c 'grep -qx green …'`) + a `File` write + a stub agent. Asserts the
  loop **converges**, the effect **landed on disk**, and it converged *because the world moved*.
- `tests/reconcile.rs::exhausts_fuel_honestly_when_agent_cannot_close_the_gap` — an agent that
  writes the wrong content: the loop must **not lie** — returns `Exhausted` carrying the residual
  red check, not a masked failure.
- `tests/llm_agent.rs::llm_agent_tool_call_drives_reconcile_to_fixed_point` — the real pipeline
  path: `LlmService::builder(mock, "mock-model").build()` → `LlmAgent` → `service.call` → drain
  `LlmEventStream` → `ToolCallEnd` → `Intent` → `File` write → converge. Proves the LLM agent
  goes through `LlmService` and that a model tool call reaches the fixed point.

---

## 6. Status & honest gaps

- **Built clean, 3/3 tests pass.** `cargo build -p tars-agent2` and `cargo clippy -p tars-agent2
  --all-targets` are warning-free.
- **Implemented:** the full core (agent/world/diff/render/effect/runtime), a deterministic
  reference world (`File` + `ShellCheck`), and the pipeline-driven `LlmAgent`.
- **Deliberately deferred (named, not hidden):**
  - `Step::Park`/`Wake` is plumbed through `Outcome::Parked` but the world-side wake-table /
    durable replay (doc 11 §8.1) is not built — a `Park` currently just suspends the run.
  - Demand-driven `unfold`/`fold`/`emphasize` paging (doc 13 §2.2): `View` renders the whole
    scope; the paging lever slots in behind the `View` type without changing the agent contract.
  - Memoized derived views (findings keyed on `Component::version`, doc 14 finding G) — the
    `Version` is present and the `Check` trait can memoize on it, but CUJ-2's memoized reviewer
    is not yet ported; only the deterministic-check path (CUJ-1) is wired end to end.
  - The tier/approval gate on handlers (doc 13 §6) is not enforced yet.
