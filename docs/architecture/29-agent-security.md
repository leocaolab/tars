# Doc 29 — Agent Security: Authorization, Exec Isolation, Cloud Integration

Status: **design** (not built). Extends Doc 02 (middleware), Doc 05 (tools),
Doc 06 (multitenancy), Doc 10 (security model), Doc 20 (agent). Grounds in the
shipped `Principal`/`Scope`, `Auth`/`SecretString`, the `Middleware` chain, and
the file-tool jail.

## 1. Overview & goal

Make a tars agent **safe to run autonomously on a server** and **safe to run
locally**, under one model:

```
authenticate (who)  →  authorize (may they?)  →  isolate (contain blast radius)
   Principal/identity      AuthzPolicy (deny-default)   ExecSandbox (fs jail + no net)
```

Three properties, non-negotiable:
1. **No interactive approval on the server** — authorization is *policy*, decided
   non-interactively (`Allow`/`Deny`, never a human prompt). Local may prompt, but
   the *default* path is the same policy.
2. **Blast radius is contained** — a hostile/confused tool call (`rm -rf`, secret
   exfiltration) cannot escape the worktree or reach the network.
3. **Portable** — the same agent code runs local / AWS / GCP; only the *seam
   implementations* (identity, secrets, sandbox) change per deployment.

**Design stance (grounded in the field survey):** don't reinvent — inherit the
cloud's primitives (workload identity, KMS secrets, gVisor/Firecracker) via seams,
and keep only the one layer the cloud can't do: **tenant↔tenant and agent↔tool
authorization**. Enforcement follows the AWS/Envoy pattern (**deny-by-default,
explicit-deny-wins, PEP/PDP split**); `rm -rf` defense follows the codex/
claude-code pattern (**OS filesystem jail on exec, not a command denylist**).

**Non-goals.** Not a WAF / not user-facing BeyondCorp; not a secrets *vault*
(we integrate one). Not a command-string denylist (structurally leaky). Not
replacing cloud IAM for tars↔cloud-resource access (we inherit it).

### 1a. Why (the two crux decisions)

- **No approval on server → policy, not prompt.** codex/claude-code lean on
  interactive approval + OS sandbox; a server agent can't prompt, so authorization
  must be a non-interactive policy decision and the sandbox must be the *primary*
  containment (it substitutes for the prompt — a contained command auto-runs).
- **`rm -rf` is a filesystem-scope problem, not a parsing problem.** tars jails
  the *file tools* in-process (`ReadFileTool::with_root` canonicalize+symlink-
  resolve, `read_file.rs:14`), but `BashTool` is `Command::new("sh")`
  (`bash.rs:110`) whose child processes the Rust layer can't police. Only an OS
  mechanism (bwrap/Seatbelt/gVisor) jails the subprocess tree. **That gap is the
  design's center of gravity.**

## 2. CUJs

- **CUJ-1 — Server autonomous run.** An agent starts on a server bound to a
  `Principal` + policy + sandbox; it runs a tool loop; every tool/RPC is authorized
  non-interactively (deny-default); exec is jailed. No human in the loop.
- **CUJ-2 — Contain `rm -rf`.** The agent (or prompt-injected content) issues a
  destructive shell command; it executes only inside the disposable worktree,
  cannot touch `$HOME`/`/`/parent-repo, and has no network.
- **CUJ-3 — No secret leak.** Secrets never land on disk in plaintext, never enter
  a prompt/log/tool-output, and the agent's tools cannot read the secret store;
  even a read secret can't be exfiltrated (egress off).
- **CUJ-4 — Tenant isolation.** Tenant A's agent cannot read/compute-over/leak into
  tenant B — enforced at the IAM middleware *and* the cache key.
- **CUJ-5 — Run on AWS.** The same agent inherits an IRSA/Fargate identity, pulls
  secrets from Secrets Manager, calls Bedrock via SigV4, and exec runs in a
  per-job Fargate/Firecracker isolate.
- **CUJ-6 — Run on GCP.** Same agent inherits a Workload-Identity SA, pulls from
  Secret Manager, calls Vertex via ADC, and exec runs in a `runtimeClass: gvisor`
  pod.
- **CUJ-7 — Run locally, bridged to cloud.** A dev runs the agent on their laptop
  using SSO/ADC short-lived credentials (no long-term keys on disk), cloud-vault
  secrets, local bwrap/Seatbelt sandbox, and actions still audit to the cloud sink.

## 3. Feature list

| # | Feature | CUJ |
|---|---|---|
| F1 | `AuthzPolicy` PDP — decide `(principal, resource, action) → Allow/Deny`, deny-default, explicit-deny-wins | 1,4 |
| F2 | `IamMiddleware` PEP — the fixed L2 pipeline layer (Doc 02) that enforces F1, cannot be bypassed | 1,4 |
| F3 | Agent-start binding — `AgentContext` carries `Principal` + policy + sandbox; tool boundary enforces the *same* F1 (unify the crude `Permissions`) | 1,2 |
| F4 | `ExecSandbox` seam — jail exec's process tree: fs write-scope + read-scope + network policy; impls: bwrap, Seatbelt, gVisor, Firecracker/Fargate | 2,5,6,7 |
| F5 | `IdentityProvider` seam — resolve `Principal` from the deployment (IRSA / GKE-WI / SSO/ADC / local static) | 5,6,7 |
| F6 | `SecretProvider` seam — resolve secrets from a vault (Secrets Manager / Secret Manager / local file), never plaintext-on-disk; reuse `SecretString` redaction | 3,7 |
| F7 | Egress policy — network off by default; allowlist via sandbox (`--unshare-net` / VPC-SC / PrivateLink) | 2,3 |
| F8 | Audit sink seam — every authz decision + tool/exec + denial emits to CloudTrail / Cloud Audit Logs / local | 1,4 |
| F9 | Config→policy loader — `config.toml [iam]` (+ external policy backend) → `Policy` → `AuthzPolicy` | 1,4 |

## 4. Requirements

**Functional**

| FR | Requirement | Feature |
|---|---|---|
| FR-1 | Policy evaluation is **deny-by-default**; an explicit `Deny` overrides any `Allow` (AWS semantics) | F1 |
| FR-2 | The IAM PEP runs **before Cache** and cannot be reordered after it (Doc 02 L2 fixed) | F2 |
| FR-3 | The **same** `AuthzPolicy` governs the LLM middleware layer *and* the agent tool/RPC boundary; a denied tool → `TaskError::Denied` (never `Ask`) on server | F1,F3 |
| FR-4 | Exec's **entire process tree** is confined: writes only under `writable_roots`, reads only under `readable_roots`, network per policy; a sandbox that can't be established → **refuse to run** (fail-closed) | F4 |
| FR-5 | `Principal` is **inherited** from the deployment identity (never minted by tars); a child agent gets a **scope subset** (`PrincipalKind::Subprocess`) | F5 |
| FR-6 | Secrets resolve from a `SecretProvider`; the raw value is a `SecretString` (redacts on Display/Debug/Serialize) and is **never** written to disk, prompt, log, or tool output | F6 |
| FR-7 | Default network egress is **denied**; allow requires an explicit policy entry | F7 |
| FR-8 | Every allow/deny + exec + secret-access emits an audit record with `principal`, `tenant`, `resource`, `action`, `decision` | F8 |

**Non-functional**

| NFR | Requirement | Feature |
|---|---|---|
| NFR-1 (security) | No single point of failure: Auth · IAM · sandbox · egress · audit are independent layers (Doc 10 defense-in-depth) | all |
| NFR-2 (fail-closed) | Any control that errors → deny/refuse, never open (missing policy → deny; sandbox init fails → no exec; secret fetch fails → no call) | F1,F4,F6 |
| NFR-3 (perf) | In-process PDP decision is sub-ms and on the hot path; sandbox setup amortized per exec, not per byte | F1,F4 |
| NFR-4 (portability) | Zero AWS/GCP symbols in `tars-*` core; cloud lives behind F4/F5/F6/F8 seams | F4,F5,F6,F8 |
| NFR-5 (blast radius) | A fully-compromised tool call cannot: write outside the worktree, reach the network, read a secret, or affect another tenant | F4,F7,F6,F2 |

## 5. Infra

| Infra | New/exists | Note |
|---|---|---|
| `Principal` / `Scope` | exists | `crates/tars-types/src/principal.rs` — *who* + *what*; `Scope` shape open for RBAC/ABAC/OPA projection |
| `Auth` / `SecretString` / `SecretRef` | exists | `auth.rs:14`, `secret.rs:45,62` — redaction + env/delegate creds |
| `Middleware`/`LlmService` chain, L2 IAM slot | slot exists, **impl new** | `tars-pipeline/src/middleware.rs:14`; Doc 02 reserves L2 "Auth & IAM, before Cache, cannot bypass" — **no iam.rs today** |
| Cache-key includes IAM scopes | exists | `tars-cache/src/key.rs:44` `IAM_SCOPES_ATTR` — cross-tenant cache defense |
| `AgentContext` (`cwd`, `readable_roots`, `permissions`) | exists, **evolve** | `tars-model/src/context.rs:17` — replace `permissions` with principal+policy+sandbox |
| File-tool jail | exists | `read_file.rs:14` / `write_file.rs:55` — the pattern to extend to exec |
| Exec sandbox | **new** | bwrap/Seatbelt/gVisor; reference impl = codex `sandboxing` crate (`SandboxType`/`SandboxPolicy`, Seatbelt SBPL, bwrap+seccomp, network proxy) |
| Cloud SDKs (STS/IRSA, Secrets Manager, Bedrock; GKE-WI, Secret Manager, Vertex) | **new, per-deployment** | behind F5/F6 seams; not in core |

## 6. Components

### C1 — `AuthzPolicy` (PDP) — `tars-types` (new `authz.rs`)

Responsibility: the decision. Codec-neutral over `Principal`/`Scope`.

Reuses: `Principal`, `Scope` (`principal.rs`), `Decision` (`permission.rs` — but
drop `Ask` for the server path). New trait.

```rust
pub enum AuthzDecision { Allow, Deny { reason: String } }   // no Ask on server
pub trait AuthzPolicy: Send + Sync {
    /// deny-by-default; an explicit Deny in any applicable rule wins (AWS semantics).
    fn decide(&self, principal: &Principal, resource: &str, action: &str) -> AuthzDecision;
}
pub struct RulePolicy { /* projected from config: roles→scopes, bindings, denies */ }
// external PDP (OPA/ext_authz) is another impl behind the same trait.
```

### C2 — `IamMiddleware` (PEP) — `tars-pipeline` (new `iam.rs`)

Responsibility: enforce C1 on every LLM call at the **fixed L2 slot** Doc 02
reserves (before Cache, un-bypassable). This is the Envoy-ext_authz / gRPC-authz-
interceptor analogue.

Reuses: `Middleware`/`LlmService` (`middleware.rs:14`, `service.rs:26`); resolves
principal from `RequestContext`; on `Deny` returns a typed error before the inner
service (Cache/Provider) runs.

### C3 — Agent-start binding + tool-boundary PEP — `tars-model` / `tars-runtime`

Responsibility: `AgentContext` binds identity + policy + sandbox at construction;
the tool loop authorizes each tool via the **same** C1 (unifying the crude
`Permissions`).

Reuses: `AgentContext` (`context.rs:17`); the existing dispatch check
(`worker.rs:530`); `TaskError::Denied` (`agent.rs`). New:

```rust
pub struct AgentContext {
    pub principal: Principal,            // was: permissions: Permissions
    pub policy: Arc<dyn AuthzPolicy>,
    pub sandbox: Arc<dyn ExecSandbox>,
    pub cwd: Option<PathBuf>,            // writable root
    pub readable_roots: Vec<PathBuf>,
    pub cancel: CancellationToken,
    pub trajectory_id: Option<String>,
}
```

### C4 — `ExecSandbox` seam — `tars-tools` (new `sandbox/`)

Responsibility: jail an exec's **process tree** — the `rm -rf` defense.

Reuses: nothing (new); `AgentContext.cwd`/`readable_roots` feed the policy.

```rust
pub struct SandboxPolicy {
    pub writable_roots: Vec<PathBuf>,   // = [ctx.cwd]
    pub readable_roots: Vec<PathBuf>,   // = [ctx.cwd] + ctx.readable_roots
    pub network: NetworkPolicy,         // Off (default) | Allowlist(..) | Full
}
pub trait ExecSandbox: Send + Sync {
    /// Wrap so writes are confined to writable_roots, reads to readable_roots,
    /// network per policy — inherited by ALL child processes. Fail-closed.
    fn wrap(&self, program: &str, args: &[String], policy: &SandboxPolicy)
        -> Result<tokio::process::Command, SandboxError>;
}
// impls: BubblewrapSandbox (linux), SeatbeltSandbox (macos),
//        GvisorSandbox (server/GKE), FargateSandbox (per-job microVM).
```
`BashTool` holds an `Arc<dyn ExecSandbox>`; `BashTool::new()` stays `NoSandbox`
for trusted contexts (mirrors file tools' `new()` vs `with_root()`).

### C5 — `IdentityProvider` / `SecretProvider` / `AuditSink` seams — `tars-runtime`

Responsibility: inherit cloud identity, fetch vault secrets, emit audit — the
portability seams.

Reuses: `Principal`, `SecretString`/`SecretRef` (`secret.rs`), the event store.

```rust
pub trait IdentityProvider { fn resolve(&self) -> Result<Principal>; }        // IRSA/GKE-WI/SSO/static
pub trait SecretProvider   { fn get(&self, r: &SecretRef) -> Result<SecretString>; } // SecretsMgr/SecretMgr/file
pub trait AuditSink        { fn record(&self, ev: &AuthzEvent); }             // CloudTrail/CloudAudit/local
```

### C6 — Config→policy loader — `tars-config`

`config.toml [iam]` (roles, bindings, denies, `default = "deny"`) → `Policy` →
`RulePolicy`. Reuses the TOML loader; adds an `[iam]` section (local source) and
a hook for an external policy backend (prod). Policy is version-controlled config
(GCP config-as-code; ties to the "git is the store" ethos).

## 7. Interfaces with other modules

- **→ pipeline**: `IamMiddleware: Middleware` inserted at L2; consumes
  `RequestContext { principal, tenant, scopes }`; denies before `CacheLookupMiddleware`.
- **→ cache**: unchanged — cache key already folds IAM scopes (`key.rs:44`), so a
  post-IAM cache hit can't cross scopes.
- **→ agent**: `AgentContext` (C3) replaces `permissions` with `principal`+`policy`
  +`sandbox`; `Agent::run` unchanged in signature.
- **→ tools**: `BashTool`/exec tools take `Arc<dyn ExecSandbox>`; file tools keep
  their in-process jail (defense in depth with the OS jail).
- **→ deployment**: `IdentityProvider`/`SecretProvider`/`AuditSink`/`ExecSandbox`
  impls are wired at process start (local vs AWS vs GCP), core unchanged.

## 8. Main algorithms

**Policy decision (AWS semantics, FR-1):**
```
applicable = rules matching (principal.scopes, resource, action)
if any applicable is Deny        -> Deny            # explicit deny wins
elif any applicable is Allow     -> Allow
else                             -> Deny            # deny-by-default
```

**Agent-start binding (CUJ-1):**
```
principal = identity_provider.resolve()             # inherit IRSA/WI/SSO
policy    = load_policy(config[iam] | external)     # deny-default
sandbox   = pick_sandbox(deployment)                # bwrap/seatbelt/gvisor/fargate
ctx = AgentContext { principal, policy, sandbox, cwd=worktree, readable_roots, ... }
```

**Tool dispatch (CUJ-1/2), non-interactive:**
```
d = ctx.policy.decide(&ctx.principal, tool.resource(), tool.action())
if d == Deny: audit(Deny); return TaskError::Denied      # never Ask on server
if tool is exec:
    cmd = ctx.sandbox.wrap(program, args, SandboxPolicy::from(&ctx))?  # fail-closed
    run cmd                                                            # rm -rf contained
```

**Secret resolution (CUJ-3):** `secret_provider.get(ref) -> SecretString`; the raw
bytes exist only inside `Auth`/the provider HTTP layer; `Display/Debug/Serialize`
redact (`secret.rs`); never placed in a prompt, event body, or tool output.

Edge cases: missing policy rule → deny (not open); sandbox unavailable on the OS →
refuse exec (don't fall back to raw `sh`); secret fetch failure → fail the call
(don't proceed unauthenticated); symlink/`..` in an exec path → the OS jail (not
the Rust check) is authoritative for the child tree.

## 9. Integration / E2E tests

- **E2E-1 (CUJ-2)**: agent runs `rm -rf $HOME` via BashTool under `BubblewrapSandbox`
  with `writable_roots=[worktree]`, net off → `$HOME` intact, only worktree-scoped
  deletion; assert files outside worktree survive and a network attempt fails.
- **E2E-2 (CUJ-1/4)**: `deny_all` policy + a tool whose scope isn't granted →
  `TaskError::Denied`, no `Ask`, audit record emitted; a granted tool runs.
- **E2E-3 (FR-2)**: attempt to place `IamMiddleware` after Cache → rejected/locked;
  a denied request never reaches the cache/provider (counter asserts).
- **E2E-4 (CUJ-3)**: a secret in config never appears in: serialized config, event
  body, tool output, or logs (assert `<redacted>`); with egress off, a tool that
  read a secret cannot POST it out.
- **E2E-5 (FR-4 fail-closed)**: force sandbox init to fail → exec refuses (no raw
  `sh` fallback).
- **E2E-6 (CUJ-5/6, integration)**: `IdentityProvider` stub returns an IRSA-shaped /
  WI-shaped principal; `SecretProvider` stub returns from a mock vault; assert the
  agent binds and authorizes without any long-term key on disk.

## 10. Success criteria

- A hostile tool call cannot escape the worktree, reach the network, read a secret,
  or touch another tenant (E2E-1/4; NFR-5).
- Server path has **no** `Ask`/prompt code reachable; deny is a typed error (E2E-2).
- IAM PEP is un-bypassable and pre-cache (E2E-3).
- No long-term secret on disk in any deployment; redaction holds everywhere (E2E-4).
- One agent binary runs local/AWS/GCP by swapping only seam impls (E2E-6; NFR-4).

## 11. Performance considerations

In-process PDP is a hashmap/scope match — sub-ms, hot-path safe (NFR-3). Sandbox
setup (bwrap/sandbox-exec spawn, or gVisor runtimeClass) is per-exec, not per-call;
amortize by reusing a sandboxed session for a tool loop where safe. External PDP
(OPA/ext_authz) adds a network hop — cache decisions per (principal,resource,action)
within a turn.

## 12. Reliability considerations

Everything fail-closed (NFR-2): missing policy → deny; sandbox init error → refuse
exec; secret fetch error → fail the call; identity unresolved → no principal → deny.
No control has an "open on error" branch (grep gate: no `unwrap_or(Allow)`,
no raw-`sh` fallback). Child-process containment must be **inherited + irreversible**
(Landlock `restrict_self` / Seatbelt profile / bwrap namespace) so a spawned
grandchild can't drop the jail.

## 13. Security considerations (the point)

- **Trust boundaries** (Doc 10 §3): LLM output = untrusted (prompt injection);
  MCP/external tool = partially trusted (isolate); tenant input = untrusted (IAM
  gate). This doc hardens the LLM-output→exec edge.
- **No-hack** = deny-by-default authz + OS exec jail + fail-closed + no raw-sh
  fallback. `rm -rf` is contained structurally (fs scope), not by parsing.
- **No-leak** = `SecretString` redaction everywhere + secrets never on disk
  (vault-fetched, short-lived) + agent tools can't read the secret store (scope +
  jail) + **egress off by default** (the exfil channel is closed even if a secret
  is read) + tenant isolation at IAM *and* cache key.
- **Provenance/keys**: prefer cloud identity (SigV4/ADC) over API keys → no
  long-lived credential to steal.
- **Audit**: every decision + exec + secret access recorded (non-repudiation).

## 14. Abstraction & reuse

**Abstraction.** Three orthogonal seams — **decide** (`AuthzPolicy`), **contain**
(`ExecSandbox`), **inherit** (`IdentityProvider`/`SecretProvider`/`AuditSink`) —
so policy backend, sandbox mechanism, and cloud are all pluggable; core stays
portable (NFR-4). This is the same "seam not policy" pattern as the decode Codec,
the cassette provider, and the bless codec.

**Reuse map.**

| Symbol | file:line | Use |
|---|---|---|
| `Principal` / `Scope` / `PrincipalKind::Subprocess` | `tars-types/src/principal.rs` | identity + scope attenuation for child agents |
| `Decision` (Allow/Deny/Ask) | `tars-model/src/permission.rs` | reuse Allow/Deny; drop Ask on server path |
| `Auth` / `SecretString` / `SecretRef` | `tars-types/src/auth.rs:14`, `secret.rs:45,62,83` | creds + redaction for no-leak |
| `Middleware` / `LlmService` | `tars-pipeline/src/middleware.rs:14`, `service.rs:26` | IAM PEP at L2 |
| IAM scopes in cache key | `tars-cache/src/key.rs:44` | cross-tenant cache defense (already correct) |
| `AgentContext` / `readable_roots` | `tars-model/src/context.rs:17` | feeds SandboxPolicy + tool authz |
| file-tool jail (`with_root`, canonicalize+symlink) | `read_file.rs:14`, `write_file.rs:55` | in-process jail; OS jail extends it to exec |
| `BashTool` (`Command::new("sh")`) | `tars-tools/src/builtins/bash.rs:110` | the site to wrap with `ExecSandbox` |
| `TaskError::Denied` | `tars-model/src/agent.rs` | non-interactive deny result |
| codex `sandboxing` crate (Seatbelt SBPL / bwrap+seccomp / network proxy) | `/Users/hucao/projects/codex/codex-rs/sandboxing/` | reference impl for C4 |

**Genuinely new**: `AuthzPolicy`+`RulePolicy`, `IamMiddleware`, `ExecSandbox`+impls,
`IdentityProvider`/`SecretProvider`/`AuditSink` seams, `[iam]` config→policy loader,
and the `AgentContext` evolution (principal+policy+sandbox replacing `Permissions`).

## Roadmap

- **M0 — authz core + deny-default (local, no cloud).** C1 `AuthzPolicy`+`RulePolicy`,
  C6 `[iam]` loader, flip default to Deny. Verify: **E2E-2**. Hardest-first: the
  Permissions→policy unification.
- **M1 — exec jail (`rm -rf` defense).** C4 `ExecSandbox` + `BubblewrapSandbox`
  (linux) + `SeatbeltSandbox` (mac); wire `BashTool`; fail-closed. Verify: **E2E-1, E2E-5**.
  *This is the highest-value local milestone — closes the bash hole.*
- **M2 — IAM PEP + agent binding.** C2 `IamMiddleware` at L2; C3 `AgentContext`
  evolution + tool-boundary PEP. Verify: **E2E-3**.
- **M3 — no-leak.** C5 `SecretProvider` + egress-off default + redaction audit.
  Verify: **E2E-4**.
- **M4 — cloud seams.** C5 `IdentityProvider`/`AuditSink`; one cloud first
  (GCP gVisor runtimeClass is the cheapest sandbox; AWS Fargate-per-job). Verify:
  **E2E-6**.
- **M5 — service mTLS + external PDP.** ALTS-equivalent RPC auth (mesh/ACM-PCA);
  OPA/ext_authz `AuthzPolicy` impl. Deferred until multi-service prod.

M0→M1 is the minimal "fence the agent + stop `rm -rf` locally" slice; M2→M3 makes
it server-safe; M4 makes it cloud-native.
