# Doc 31 — AWS Bedrock Provider (keyless, IAM/SigV4, unified Converse API)

> Status: **M0+M1 built** (feature-gated `--features bedrock`; Converse mapping + keyless
> SigV4 client + real `ConverseStream`; §4/§6 corrected as-built — see the as-built notes there).
> 2026-07-05. Extends Doc 01 (LlmProvider), Doc 29
> (agent security / cloud identity seams), Doc 30 (which explicitly reserves
> Bedrock as its own `LlmProvider` impl, not an OpenAI dialect). Grounds in the
> shipped `LlmProvider`/`ProviderRegistry`/`ProviderConfig`/`Auth` and the
> anthropic backend's mapping↔adapter↔provider split.

## 1. Overview & goal

Add a **`bedrock` provider** that gives tars **keyless, IAM/SigV4-authenticated**
access to Bedrock-hosted models (Claude, Nova, Llama, Mistral, Cohere, …) through
Bedrock's **unified `Converse` / `ConverseStream` API**. This is the
production/cloud path: on AWS the workload's own identity (IRSA / EC2 instance
role / ECS task role / SSO) signs every request — there is **no long-lived API
key on disk**, which is exactly the cloud-identity stance Doc 29 §1/CUJ-5 sets out
("prefer cloud identity (SigV4/ADC) over API keys → no key material at rest").

One mapping (`ChatRequest`↔Converse) covers **all** Bedrock models, because
Converse is Bedrock's cross-model normalization layer — we do not write per-model
request bodies and we do not reuse the Anthropic Messages mapping. Credential
resolution and request signing are delegated wholesale to the AWS SDK
(`aws-config` + `aws-sdk-bedrockruntime`); tars hand-rolls **no** SigV4.

**Non-goals.**
- **Not** reusing the `anthropic`/`gemini`/`openai` mappings. Converse is a
  distinct, already-unified wire shape; forcing it through the Anthropic Messages
  mapping (the InvokeModel-Claude alternative, §3) would be Claude-only and
  per-model. Bedrock is a **separate `LlmProvider`** that coexists with them; the
  only shared surface is the canonical `ChatRequest`/`ChatResponse`/`ChatEvent`.
- **Not** hand-rolling SigV4 or a credential chain. The AWS SDK owns both.
- **Not** Bedrock embeddings or image generation in v1 (§9 M2 defers them).
- **Not** Gemini→Vertex. That is the sibling GCP path (same "keyless via cloud
  identity" shape); it is a separate future doc, mentioned here only as a parallel.
- **Not** a general `Auth` signer variant *in v1*. We ship Bedrock against the AWS
  cred chain first; generalizing `Auth` to carry a credential-provider/signer
  (Doc 29 `IdentityProvider`) is M3, driven by this concrete second consumer.

## 2. Critical User Journeys (CUJs)

- **CUJ-1 — Claude on Bedrock with zero API key.** Actor: an operator with AWS
  credentials available to the process (env / `~/.aws` profile / instance role).
  Trigger: config declares `type = "bedrock"`, `region`, `model =
  "us.anthropic.claude-...-v2:0"`, **no `auth` field**. Steps: registry builds the
  provider → first `complete()` resolves the AWS cred chain via `aws-config` →
  `Converse` call is SigV4-signed by the SDK → response mapped to `ChatResponse`.
  Success: a completion returns with correct text + usage and **no key was ever
  read from tars config**.
- **CUJ-2 — One agent routes Claude-direct ↔ Claude-on-Bedrock.** Actor: a tars
  agent/pipeline. Trigger: routing selects provider id `claude_bedrock` vs
  `anthropic_main` for the same logical request. Steps: the caller builds one
  canonical `ChatRequest`; each provider maps it its own way. Success: identical
  `ChatRequest` in, canonical `ChatResponse` out, caller code unchanged — the
  provider identity is the *only* difference.
- **CUJ-3 — Streaming via ConverseStream.** Actor: a streaming consumer. Trigger:
  `provider.stream(req, ctx)`. Steps: `ConverseStream` yields SDK stream events →
  mapped one-to-one to `ChatEvent` (`Started` / `Delta` / `ThinkingDelta` /
  `ToolCallStart/ArgsDelta/End` / `Finished`). Success: the same event contract as
  every other provider; tool-call args assemble across deltas.
- **CUJ-4 — On AWS, the workload identity signs automatically.** Actor: tars
  running under IRSA (EKS) / an ECS task role / an EC2 instance role. Trigger: any
  Bedrock call. Steps: `aws-config`'s default chain discovers the ambient role, no
  config change from the laptop case. Success: the same `[providers.x]` block that
  used a local profile now signs with the pod/instance identity — portability with
  zero tars-side credential handling (Doc 29 CUJ-5).
- **CUJ-5 — A Bedrock failure tells the truth.** Actor: any caller. Trigger: an
  `AccessDeniedException` / throttling / model-not-enabled error from the SDK.
  Steps: the SDK error is classified into a typed `ProviderError` **carrying the
  service message** (CLAUDE.md #1 — no sentinel). Success: the operator sees the
  real AWS message (e.g. "you don't have access to the model with the specified
  model ID"), classified as `Auth` / `RateLimited` / `InvalidRequest`.

## 3. Key decision — Converse (unified) vs InvokeModel (per-model + reuse Anthropic)

Two ways to reach Bedrock:

| Axis | **Converse / ConverseStream** (chosen) | InvokeModel + reuse Anthropic mapping |
|---|---|---|
| Model coverage | **All** Bedrock models via one normalized shape | Claude-only; every other family needs its own body |
| Mapping count | **One** `ChatRequest`↔Converse mapping | One per model family (Claude, Nova, Llama, …) |
| Reuse of `anthropic/mapping.rs` | None (different wire shape) | Reuses the Messages body — but only for Claude |
| SigV4 | SDK signs (typed client) | SDK signs (`invoke_model` on the same client) or hand-rolled over reqwest |
| New dep weight | Heavy: `aws-sdk-bedrockruntime` + `aws-config` + smithy stack | Same SDK if used typed; or hand-rolled SigV4 (worse) |
| Envelope handling | SDK gives typed `Converse*` structs | Hand-roll each model's request/response envelope JSON |
| Streaming | `ConverseStream` typed event union | Per-model SSE/event-stream framing, hand-parsed |

**Decision: Converse**, matching rig-bedrock. The InvokeModel-reuse-Anthropic path
buys us one shared Claude body at the cost of (a) locking Bedrock to Claude, (b)
re-deriving a per-model envelope for Nova/Llama/Mistral/Cohere, and (c)
hand-parsing per-model streaming frames. Converse pays a single mapping and lets
the SDK normalize every model family — the reuse we give up (`anthropic/mapping.rs`)
is Claude-specific and would not have covered the other families anyway.

**The honest cost:** `aws-sdk-bedrockruntime` + `aws-config` + `aws-smithy-*` +
`aws-runtime` pull in a large dependency subtree (hyper/rustls, the smithy
runtime, credential providers). §4 isolates that weight in a **separate crate** so
the core `tars-provider` build is unaffected unless Bedrock is actually compiled in.

## 4. Crate placement — separate `tars-bedrock` crate

**Decision: a new workspace crate `crates/tars-bedrock`**, not a module inside
`tars-provider`, and not a Cargo feature on `tars-provider`. Reasons:

1. The AWS SDK subtree is heavy; folding it into `tars-provider` taxes every build
   that touches providers, even ones that never use Bedrock. rig makes the same
   call (`rig-bedrock` is a separate crate).
2. A Cargo feature (`tars-provider/bedrock`) would keep the deps optional but still
   couples the SDK's MSRV/version churn to the core provider crate and litters it
   with `#[cfg(feature)]`. A crate boundary is cleaner and the registry already
   knows how to depend outward.
3. **As-built correction (M0): `tars-bedrock` is a TRUE leaf — it depends ONLY on
   `tars-types`.** The original sketch had it also depend on `tars-provider` (for
   `LlmProvider`) *and* have `tars-provider` gain a `bedrock` feature pulling in
   `tars-bedrock` — that is a `tars-provider ↔ tars-bedrock` **Cargo cycle**, which
   Cargo rejects and which broke the workspace build. Resolution: keep ALL AWS-specific
   logic (Converse mapping, `Document` shim, client) in the leaf `tars-bedrock`, and
   host the ~90-line `impl LlmProvider for BedrockProvider` **bridge in
   `tars-provider::backends::bedrock`** (a local trait impl for the foreign
   `tars_bedrock` types). `tars-provider` gets `bedrock = ["dep:tars-bedrock"]`; the
   single registry arm is `#[cfg(feature = "bedrock")]` (feature-off → a
   `RegistryError::FeatureDisabled`). Cycle-free, AWS SDK stays out of the default graph.

**Dependency list for `crates/tars-bedrock/Cargo.toml` (as-built, leaf — canonical
types ONLY; no `tars-config`, no `tars-provider`):**

```toml
[dependencies]
# Canonical types only. This crate must NOT depend on tars-provider: the
# `impl LlmProvider` bridge lives in tars-provider (behind its `bedrock`
# feature), so tars-provider can depend on this leaf without a cycle.
tars-types = { path = "../tars-types" }

aws-config             = { version = "1", features = ["behavior-version-latest"] }
aws-sdk-bedrockruntime = { version = "1", features = ["rt-tokio"] }
aws-smithy-types       = "1"                     # Document (tool JSON), Blob

tokio                  = { workspace = true }
futures.workspace      = true
async-stream.workspace = true                    # ConverseStream recv-loop → event generator (M1)
serde_json.workspace   = true

[dev-dependencies]
tokio     = { workspace = true, features = ["macros", "rt-multi-thread"] }
serde_json.workspace = true
```

(No `async-trait`/`tracing`/`thiserror` in the leaf — the `#[async_trait] impl
LlmProvider` and `#[tracing::instrument]` live in the
`tars-provider::backends::bedrock` bridge. The AWS SDK uses the **default** feature
set + `rt-tokio`, not the sketch's `default-features = false, … "rustls"`.)

Add `crates/tars-bedrock` to `[workspace].members` in `Cargo.toml:3`; keep it out
of `default-members` (`Cargo.toml:36`) initially so `cargo build --workspace`
without the SDK toolchain stays fast, exactly like `tars-py` is excluded there.

## 5. Auth — the AWS credential chain, not `Auth::Secret`

Today `tars_types::Auth` is `None | Delegate | Secret{SecretRef}`
(`crates/tars-types/src/auth.rs:14`) and the runtime resolves it to
`ResolvedAuth::{ApiKey|Bearer|None}` (`crates/tars-provider/src/auth.rs:108`). None
of these fit Bedrock: there is no key to inject into a header — a **request signer**
derived from a **credential provider** signs the whole request.

**v1 stance: Bedrock does not use `Auth` at all.** `ProviderConfig::Bedrock` has
**no `auth` field**. Credential resolution is delegated to `aws-config`'s default
provider chain, which already implements the standard precedence (env vars →
`AWS_PROFILE` / `~/.aws/config` → SSO → ECS/EKS container credentials → EC2 IMDS
instance role). The optional `profile` config field just names a profile for the
laptop case; on AWS it is omitted and the ambient role wins (CUJ-4).

```toml
[providers.claude_bedrock]
type   = "bedrock"
region = "us-east-1"
model  = "us.anthropic.claude-sonnet-4-5-v1:0"
# no auth field. profile is optional, laptop-only:
# profile = "dev"
```

Client construction is **async** (`aws_config::defaults(..).region(..).load().await`
then `aws_sdk_bedrockruntime::Client::new(&cfg)`), but `ProviderRegistry::from_config`
/ `build_one` are **synchronous** (`registry.rs:72`, `:166`). Resolution: the
builder stores `region`/`profile` only; the provider holds a
`tokio::sync::OnceCell<Client>` and builds the client **lazily on the first
`stream()`** call (which is already `async`). This keeps `build_one` sync, defers
the credential-chain I/O to first use, and means a misconfigured region/identity
surfaces as a classified `ProviderError` on the first call rather than at
registry-build time.

**Forward tie (M3, Doc 29 C5):** this is the concrete second driver for
generalizing `Auth` to carry a signer/credential-provider — a future
`Auth::Delegate`-like `Workload`/`Signer` variant resolved by an
`IdentityProvider` (Doc 29 `docs/architecture/29-agent-security.md:200-208`). v1
ships the AWS chain directly inside `tars-bedrock`; M3 lifts the seam so GCP-ADC
(Vertex) reuses it. We do **not** speculatively add the variant now.

## 6. Components

### C1 — `bedrock::mapping` (pure `ChatRequest`↔Converse converters, new)

- **Responsibility:** stateless, no-I/O conversion between canonical types and the
  SDK's typed Converse structs. Mirrors the role of
  `crates/tars-provider/src/backends/anthropic/mapping.rs` (map_stop_reason `:19`,
  parse_usage `:35`, message_to_chat_response `:211`) — same "pure JSON/typed
  conversion layer, tested without any transport" split.
- **Reuses:** `StopReason` (`crates/tars-types/src/events.rs:64`) for the
  stop-reason mapping; `Usage` (`crates/tars-types/src/usage.rs`) shape and the
  same normalization intent as `anthropic/mapping.rs:parse_usage`; `ChatResponse` /
  `ChatResponseBuilder` (`crates/tars-types/src/response.rs:12`) to assemble the
  non-streaming response by replaying events, exactly as
  `anthropic/mapping.rs:message_to_chat_response:211` does.
- **New:** the Converse-specific field mapping.
- **Interface:**
  ```rust
  // ChatRequest → Converse request pieces (system / messages / tool_config / inference_config).
  pub(crate) fn build_converse(req: &ChatRequest)
      -> Result<ConverseParts, ProviderError>;   // ConverseParts holds the SDK builders' inputs

  pub(crate) fn map_stop_reason(s: &aws_sdk_bedrockruntime::types::StopReason) -> StopReason;
  pub(crate) fn parse_usage(u: &aws_sdk_bedrockruntime::types::TokenUsage) -> Usage;

  // Non-streaming: AwsConverseOutput → ChatResponse (replayed through ChatResponseBuilder).
  pub(crate) fn converse_output_to_response(
      out: &aws_sdk_bedrockruntime::operation::converse::ConverseOutput,
  ) -> Result<ChatResponse, ProviderError>;
  ```
  Mapping specifics: `ChatRequest.system` → `Vec<SystemContentBlock::Text>`;
  `Message::{User,Assistant,Tool}` (`tars-types` chat) → Converse `Message` with
  `ContentBlock::{Text, Image, ToolUse, ToolResult}`; `req.tools` → `ToolConfiguration`
  (each `ToolSpec.input_schema.schema` → `aws_smithy_types::Document` via a
  `serde_json::Value → Document` shim); `req.tool_choice` → `ToolChoice`;
  `req.temperature`/`req.max_output_tokens`/`req.stop_sequences` →
  `InferenceConfiguration`; `req.thinking` → the model-agnostic
  `additionalModelRequestFields` reasoning knob where the model supports it.

### C2 — `bedrock::stream` (ConverseStream event → `ChatEvent`, new)

- **Responsibility:** consume the SDK's `ConverseStreamOutput` event union and emit
  canonical `ChatEvent`s, assembling tool-call argument fragments across deltas.
- **As-built (leaf constraint):** the crate depends ONLY on `tars-types`, so it
  **cannot** reuse `tars-provider`'s `ToolCallBuffer`. Instead `stream.rs`
  reimplements a small **local accumulator** — a `StreamTranslator` holding a
  `HashMap<i32, ToolAccum>` keyed by Bedrock's `content_block_index`, where
  `ToolAccum { id, input: String }` concatenates the partial `input` JSON fragments
  and parses them **once at `ContentBlockStop`** (the same "never parse mid-stream"
  invariant `ToolCallBuffer` enforces, Doc 01 §8.1 — just re-expressed locally, ~a
  dozen lines). A malformed `input` at block-stop is a real
  `ProviderError::Parse` carrying the raw fragment (CLAUDE.md #1).
- **Reuses (from `tars-types` only):** `ChatEvent` (`events.rs:13`), `StopReason`,
  `Usage`, `ProviderError`; the local `map_stop_reason` / `parse_usage` from
  `bedrock::mapping` (C1).
- **New:** the `StreamTranslator` + the SDK-event → `ChatEvent` match (structurally
  the twin of `anthropic/adapter.rs:parse_event`, but over typed SDK events instead
  of SSE JSON, with its own tool accumulation instead of the shared buffer).
- **Transport / interface (as-built):** the translator is pure/no-I/O; the transport
  in `client.rs` drives it. `BedrockClient::stream_response(...) ->
  Result<BedrockEventStream, ProviderError>` builds the stream with
  `async_stream::try_stream! { … }` — `BedrockEventStream` is a boxed
  `Stream<Item = Result<ChatEvent, ProviderError>>` defined **in this leaf crate**
  (the `tars-provider::bedrock` bridge maps it into `LlmEventStream`; the leaf never
  names `boxed_stream`/`LlmEventStream`).
  ```rust
  // Pure, incremental — one SDK event → 0..N canonical events.
  pub struct StreamTranslator { /* tools: HashMap<i32, ToolAccum>, stop_reason, usage, finished */ }
  impl StreamTranslator {
      pub fn new() -> Self;
      pub fn translate(&mut self, ev: ConverseStreamOutput) -> Result<Vec<ChatEvent>, ProviderError>;
      pub fn finish(&mut self) -> Option<ChatEvent>;   // terminal Finished(stop_reason, usage)
  }
  ```
  Event map: `MessageStart` → (nothing here — `Started` is surfaced by the transport);
  `ContentBlockStart{ToolUse}` →
  `ToolCallStart` + insert a `ToolAccum` for that block index; `ContentBlockDelta{Text}` → `Delta`;
  `ContentBlockDelta{ReasoningContent}` → `ThinkingDelta`;
  `ContentBlockDelta{ToolUse{input}}` → `ToolCallArgsDelta` + append to the block's `ToolAccum.input`;
  `ContentBlockStop` → parse the accumulated `input` → `ToolCallEnd` (only for a
  block that has a live `ToolAccum`); `MessageStop{stop_reason}` records
  `stop_reason`, `Metadata{usage}` records `usage`, and the terminal `Finished` is
  emitted by `StreamTranslator::finish()` at stream end (a synthetic terminator if
  the stream ends without one — same fail-closed intent as the shared buffer's
  `recovered_finish_state`).

### C3 — `bedrock::provider` (`BedrockProvider` + builder, new)

- **Responsibility:** provider lifecycle; lazy AWS client; `impl LlmProvider`.
- **Reuses:** `LlmProvider` trait (`crates/tars-provider/src/provider.rs:28`) —
  implements `id`/`capabilities`/`stream`; inherits the default `complete`
  (`:52`), `count_tokens` fast estimate (`:71`), and `cost` (`:96`). The builder +
  `default_capabilities()` shape mirrors
  `crates/tars-provider/src/backends/anthropic/provider.rs:30,78,100`.
  `#[tracing::instrument(name = "bedrock.stream", …)]` per `provider.rs:121`.
- **New:** the lazy `OnceCell<Client>` construction from `region`/`profile`.
- **Interface:**
  ```rust
  pub struct BedrockProviderBuilder { id: ProviderId, region: String, model: String, profile: Option<String>, capabilities: Option<Capabilities> }
  impl BedrockProviderBuilder {
      pub fn new(id: impl Into<ProviderId>, region: String, model: String) -> Self;
      pub fn profile(self, p: Option<String>) -> Self;
      pub fn capabilities(self, c: Capabilities) -> Self;
      pub fn build(self) -> Arc<BedrockProvider>;   // NB: no HttpProviderBase / AuthResolver needed
  }

  pub struct BedrockProvider { /* id, model, region, profile, caps, client: OnceCell<Client> */ }
  #[async_trait] impl LlmProvider for BedrockProvider {
      fn id(&self) -> &ProviderId;
      fn capabilities(&self) -> &Capabilities;
      async fn stream(self: Arc<Self>, req: ChatRequest, ctx: RequestContext)
          -> Result<LlmEventStream, ProviderError>;
      // complete / count_tokens / cost: trait defaults.
  }
  ```
  Non-streaming note: because `converse()` (unary) is strictly cheaper than
  `converse_stream()` for the aggregate case, **override `complete()`** to call
  `converse()` + `converse_output_to_response` (C1) — the trait doc at
  `provider.rs:44` explicitly invites this ("override only if the provider has a
  non-streaming fast-path that's strictly better").

### C4 — Config + registry wiring

- **Responsibility:** declare `ProviderConfig::Bedrock` and build it.
- **Reuses / touches:**
  - `crates/tars-config/src/providers.rs:95` — add a `Bedrock` variant to the
    `#[serde(tag = "type")]` enum (fields: `region`, `model`, `profile:
    Option<String>`; **no `auth`**). Mirror the trimmed-string serde helpers
    (`de_trimmed_string` `:22`, `de_trimmed_opt_string` `:33`).
  - `providers.rs:528` `default_model()` — add `Bedrock { model, .. } => model`.
  - `providers.rs:549` `validate_self()` — add an arm: `region` non-empty, `model`
    non-empty; **must NOT** require auth (contrast the Anthropic/OpenAI/Gemini arms
    at `:637/:606/:662` which reject `Auth::None`). This is the config-level
    expression of "Bedrock is keyless".
  - `crates/tars-provider/src/registry.rs:166` `build_one` — add
    `#[cfg(feature = "bedrock")] ProviderConfig::Bedrock { region, model, profile } =>
    tars_bedrock::BedrockProviderBuilder::new(id, region.clone(), model.clone())
    .profile(profile.clone()).build()`. The arm ignores the `http` and
    `auth_resolver` args (Bedrock needs neither) — acceptable; several arms already
    ignore `default_model`. Under `#[cfg(not(feature = "bedrock"))]` the arm returns
    a clear `RegistryError` telling the operator to rebuild with the feature.
- **New:** the enum variant, the validate arm, the build arm, and the
  `bedrock` feature on `tars-provider/Cargo.toml` forwarding to `tars-bedrock`.

## 7. Interfaces with other modules

| Direction | Module | Symbol / signature (`file:line`) | Purpose |
|---|---|---|---|
| implements → | `tars-provider` | `trait LlmProvider` (`provider.rs:28`); `stream`(`:37`), default `complete`(`:52`) | Bedrock is a first-class provider |
| calls → | `tars-provider` | `ToolCallBuffer` (`tool_buffer.rs:31`), `boxed_stream` (`provider.rs:121`) | assemble streamed tool-call args |
| returns → | `tars-types` | `ChatResponse`/`ChatResponseBuilder` (`response.rs:12`), `ChatEvent` (`events.rs:13`), `Usage` (`usage.rs`), `StopReason` (`events.rs:64`), `ProviderError` | canonical output contract |
| consumes ← | `tars-types` | `ChatRequest` (`chat.rs:15`), `Message`/`ContentBlock`/`ToolSpec`/`ToolChoice` | canonical input contract |
| built by ← | `tars-provider` | `registry::build_one` (`registry.rs:166`) | config → live provider |
| declared in ← | `tars-config` | `ProviderConfig` (`providers.rs:95`), `validate_self` (`:549`), `default_model` (`:528`) | TOML surface |
| external → | AWS SDK | `aws_config::defaults().load()`, `aws_sdk_bedrockruntime::Client::{converse, converse_stream}` | credential chain + SigV4 + transport |
| future ↔ | `tars-runtime` | `IdentityProvider` seam (Doc 29 C5, `29-agent-security.md:200-208`) | M3: lift credential resolution behind the cloud seam |

**Explicitly NOT crossed:** `HttpProviderBase` / `HttpAdapter` / `stream_via_adapter`
(`http_base.rs:73,100,165`) and `AuthResolver`/`ResolvedAuth` (`auth.rs:108,128`).
Bedrock's transport and auth are the AWS SDK's; the shared reqwest/SSE base does not
apply. This asymmetry (a provider that ignores `http`+`auth_resolver`) is the one
structural wrinkle and is called out in §6 C4.

## 8. Main algorithms

### 8.1 Lazy signed client (per provider, once)
```
stream(req, ctx):
  client = self.client.get_or_try_init(async {
      cfg = aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(self.region))
                .profile_name(self.profile?)      # only if Some
                .load().await                     # resolves env/profile/SSO/ECS/IMDS chain
      Client::new(&cfg)
  }).await?                                        # first-call failure → classified ProviderError
  ...
```
Invariants: the credential chain is resolved exactly once per provider instance;
`OnceCell` makes concurrent first-calls race-safe (one wins, others await). Edge
cases: no credentials discoverable → `ProviderError::Auth` with the SDK's chain
error; unknown region → surfaced on first call, not swallowed.

### 8.2 Non-streaming completion
```
complete(req, ctx):
  parts = build_converse(req)                      # C1; InvalidRequest on empty messages / bad schema
  out = client.converse()
            .model_id(self.model)
            .set_system(parts.system)
            .set_messages(parts.messages)
            .set_tool_config(parts.tool_config)
            .set_inference_config(parts.inference)
            .send().await
            .map_err(classify_sdk_error)?          # CUJ-5: carries service message
  converse_output_to_response(out)                 # replay through ChatResponseBuilder
```

### 8.3 Streaming (ConverseStream)
```
stream(req, ctx):
  parts = build_converse(req)
  resp = client.converse_stream()....send().await.map_err(classify_sdk_error)?
  buf = ToolCallBuffer::new()
  async-stream over resp.stream.recv():
    match event:
      MessageStart              -> yield Started
      ContentBlockDelta.text    -> yield Delta
      ContentBlockDelta.reason  -> yield ThinkingDelta
      ContentBlockStart.toolUse -> buf.on_start; yield ToolCallStart
      ContentBlockDelta.toolUse -> buf.on_delta; yield ToolCallArgsDelta
      ContentBlockStop          -> if buf.is_inflight: yield ToolCallEnd(buf.finalize)
      MessageStop{stop_reason}  -> buf.record_finish_state
      Metadata{usage}           -> yield Finished(stop_reason, parse_usage)
    on recv error mid-stream    -> yield Err(classify_sdk_error)   # never panic (provider.rs:9)
  # synthetic terminator if stream ends without a Finished (buf.recovered_finish_state), same as adapter.rs:559
```
Invariants: exactly one terminal `Finished` per successful stream (mirrors
`events.rs` contract); tool-call args are parsed only at `ContentBlockStop`, never
mid-fragment (mirrors `events.rs:37` "Do not parse mid-stream").

### 8.4 Error classification (`classify_sdk_error`)
```
map SdkError::ServiceError(e):
  AccessDeniedException | UnauthorizedException     -> ProviderError::Auth(msg)
  ThrottlingException                               -> ProviderError::RateLimited{ retry_after: None }
  ModelNotReadyException | ServiceUnavailable       -> ProviderError::ModelOverloaded
  ValidationException                               -> ProviderError::InvalidRequest(msg)   # incl. model-not-enabled
  ModelTimeoutException                             -> ProviderError::Internal(msg)
  other                                             -> ProviderError::Internal(msg)
map SdkError::{Timeout,Dispatch,Construction}       -> ProviderError::Internal(msg carrying the source)
```
`msg` is always the SDK's own message string (CLAUDE.md #1: carry the raw truth,
no `parse_failed`/`unknown` sentinel). Context-overflow, if Bedrock reports it as a
`ValidationException`, stays `InvalidRequest` (we do not fabricate token counts we
don't have).

## 9. Integration / E2E tests

Real Bedrock needs live AWS credentials + model access, so the default suite is a
**mock-shaped mapping test** (no network), with a **gated live-smoke** mirroring
the other providers' `*_integration.rs` (env-gated).

| Test | CUJ | Setup → Action → Assertion |
|---|---|---|
| E2E-1 (mapping, non-stream) | CUJ-1,2 | Build a `ConverseOutput` fixture (typed struct, or JSON deserialized to it) with text + `TokenUsage` → `converse_output_to_response` → assert `ChatResponse.text`, `usage.{input,output}_tokens`, `stop_reason`. No AWS call. |
| E2E-2 (request mapping) | CUJ-2 | A `ChatRequest` with system + tools + tool_choice → `build_converse` → assert the Converse `system`/`messages`/`tool_config` are shaped correctly (tool schema → `Document`), and canonical→Converse is total (no field dropped silently). |
| E2E-3 (stream mapping) | CUJ-3 | A `Vec<ConverseStreamOutput>` fixture (MessageStart, text deltas, a tool-use block across 2 deltas, MessageStop, Metadata) fed through the C2 mapper → assert ordered `ChatEvent`s incl. one assembled `ToolCallEnd` with parsed args and exactly one `Finished`. |
| E2E-4 (error truth) | CUJ-5 | A synthetic `ValidationException{message:"…model ID…"}` → `classify_sdk_error` → assert `ProviderError::InvalidRequest` **and the message substring is present** (no sentinel). |
| E2E-5 (config/registry) | CUJ-1 | Load a `type = "bedrock"` TOML with no `auth` → `ProviderConfig::Bedrock`; assert `validate_self` yields **no** auth error and `default_model()` returns `model`; with `bedrock` feature, `build_one` returns a live `Arc<dyn LlmProvider>`. |
| E2E-6 (live smoke, gated) | CUJ-1,4 | `#[ignore]`/env-gated (`TARS_BEDROCK_LIVE=1` + region + model): real `complete()` against Bedrock using the ambient cred chain → assert non-empty text + non-zero usage. Not run in CI without creds. |

## 10. Success criteria

- [ ] All FR met: canonical `ChatRequest`→Converse→`ChatResponse` round-trips for
      text, tools, thinking, and usage; `bedrock` provider builds from a keyless
      config; streaming yields the standard `ChatEvent` contract.
- [ ] Every CUJ has a passing E2E test (§9); CUJ-6 live-smoke passes when gated on.
- [ ] `cargo build --workspace` (default-members, no `bedrock` feature) is
      **unchanged in dep graph** — the AWS SDK does not enter the default build.
- [ ] NFR thresholds (§11–13) met: no key material at rest; SDK errors carry the
      service message; no provider-name/sentinel leakage.

## 11. Performance considerations

- **Hot path:** streaming decode (per-event SDK-struct → `ChatEvent` match) — no
  JSON re-parse (the SDK already decoded event-stream frames into typed structs),
  strictly cheaper than the SSE-string path the reqwest backends run.
- **First-call cost:** credential-chain resolution + client build happen **once**
  per provider via `OnceCell` (§8.1); IMDS/SSO lookups can add ~tens of ms on the
  first call only. Measure: first-call vs warm-call latency; assert warm-call adds
  no per-call auth I/O.
- **Non-stream fast path:** `complete()` uses unary `converse()` (§6 C3), avoiding
  the stream framing overhead for the aggregate case.
- **Budget:** the mapping is allocation-light (borrows from `ChatRequest`); the
  dominant cost is the network round-trip, unchanged from any HTTP provider.

## 12. Reliability considerations

- **Failure modes:** missing/expired credentials, model-not-enabled, throttling,
  region typo, mid-stream disconnect — all classified (§8.4) and, mid-stream,
  yielded as `Err(ProviderError)` rather than panicking (`provider.rs:9`).
- **Recovery / idempotency:** a completion is a single request; retries are the
  caller's (the SDK's built-in retry/backoff handles transient throttling — keep
  its default, do not double-retry). `OnceCell` never caches a *failed* init, so a
  transient credential-endpoint blip on the first call can be retried on the next.
- **Fail-closed:** no credentials → hard `ProviderError::Auth`, never a silent
  unsigned request. A stream that ends without a `Finished` emits a synthetic
  terminator from the last-seen stop/usage (`ToolCallBuffer::recovered_finish_state`,
  `tool_buffer.rs:95`) so consumers never hang.

## 13. Security considerations

- **Trust boundary:** the AWS SDK is the only code that sees credentials; tars
  never handles key material for this provider (the whole point). No secret in
  tars config, no secret in logs — there is no `ResolvedAuth::ApiKey` to redact.
- **Least privilege:** the signing identity is the workload's IAM role; access is
  scoped by IAM policy (`bedrock:InvokeModel*`) outside tars. Doc 29's stance:
  inherit cloud IAM, don't reinvent it.
- **Validate:** `region`/`model` non-empty at config time (§6 C4); tool JSON
  schemas pass through `serde_json::Value → Document` without executing anything.
- **Log hygiene:** `#[tracing::instrument(err(Display))]` logs the classified error
  (service message), which is non-sensitive; request/response bodies are not logged
  at info. Never log the resolved `SdkConfig`/credentials.

## 14. Abstraction & reuse

**Approach.** Bedrock is a **separate `LlmProvider`** in its own crate, structured
as the same three-file split every mature tars backend uses (pure mapping / stream
+ transport / provider lifecycle), so a reader who knows `anthropic/` can read
`tars-bedrock/` immediately. The only shared surface with `anthropic`/`gemini`/
`openai` is the canonical `ChatRequest`/`ChatResponse`/`ChatEvent` — there is **no
shared mapping** (Converse is already unified; sharing the Anthropic body would be
Claude-only, §3). Routing (Doc 01/`ProviderRegistry`) picks Claude-direct vs
Claude-on-Bedrock purely by provider id (CUJ-2).

**Reuse map (existing code to call):**

| Symbol | Location | How we use it |
|---|---|---|
| `LlmProvider` (trait, default `complete`/`count_tokens`/`cost`) | `crates/tars-provider/src/provider.rs:28,52,71,96` | implement; inherit defaults; override `complete` with unary converse |
| `boxed_stream` / `LlmEventStream` | `crates/tars-provider/src/provider.rs:121,25` | box the streaming mapper output |
| `ToolCallBuffer` | `crates/tars-provider/src/tool_buffer.rs:31` (`on_start:125`,`on_delta:155`,`finalize:168`,`record_finish_state:89`,`recovered_finish_state:95`) | assemble tool-call args + synthetic terminator, identical to `anthropic/adapter.rs` |
| `ChatResponseBuilder` | `crates/tars-types/src/response.rs` | replay events → `ChatResponse` (as `anthropic/mapping.rs:211`) |
| `ChatEvent` / `StopReason` / `Usage` / `ProviderError` | `crates/tars-types/src/{events.rs:13,64,usage.rs,error.rs}` | canonical output + typed errors |
| `ChatRequest` / `Message` / `ContentBlock` / `ToolSpec` / `ToolChoice` | `crates/tars-types/src/{chat.rs:15,tools.rs}` | canonical input to map |
| three-file backend pattern | `crates/tars-provider/src/backends/anthropic/{mapping.rs,adapter.rs,provider.rs}` | structural template (mapping/stream/provider) |
| `ProviderConfig` + `build_one` + validate | `crates/tars-config/src/providers.rs:95,528,549`; `registry.rs:166` | new `Bedrock` variant + build/validate arms |
| workspace membership | `Cargo.toml:3,36` | add `crates/tars-bedrock` to members (not default-members) |

**New abstractions (justified):**
- `crates/tars-bedrock` crate — *justified* by the heavy AWS SDK dep that must not
  burden the core (§4); rig makes the same split.
- `bedrock::mapping` / `bedrock::stream` — *justified* Converse-specific logic that
  has no analogue to reuse (the anthropic mapping is a different wire shape).
- `ProviderConfig::Bedrock` (no `auth`) — *justified* first keyless HTTP-class
  provider; its validate arm intentionally omits the `Auth::None` rejection the
  other cloud providers carry. This is the config-level marker of the auth-model
  difference and the seed for the M3 `Auth` generalization.

No new abstraction is introduced where an existing trait/type would do:
`LlmProvider`, `ToolCallBuffer`, `ChatResponseBuilder`, and the canonical types are
all reused as-is; Bedrock adds a crate and a mapping, not a new provider contract.

## Roadmap

- **M0 — Crate + Converse mapping (non-streaming) + config/registry + mock test.**
  Scope: create `crates/tars-bedrock` (§4 deps), `bedrock::mapping` (C1),
  `bedrock::provider` with lazy `OnceCell<Client>` and `complete()` via unary
  `converse()`, `ProviderConfig::Bedrock` + validate + `build_one` arm behind the
  `bedrock` feature. Delivers: C1, C3, C4; FR for CUJ-1/2/5. Depends: —.
  **Risk-up-front:** the two hardest unknowns are here — the async-client-in-sync-
  registry seam (§5/§8.1) and the `Value → aws_smithy_types::Document` tool-schema
  shim. Verified by: **E2E-1, E2E-2, E2E-4, E2E-5**.
- **M1 — ConverseStream.** Scope: `bedrock::stream` (C2) + `stream()` impl reusing
  `ToolCallBuffer`. Delivers: CUJ-3. Depends: M0. Verified by: **E2E-3**; and
  **E2E-6** (gated live smoke) once creds are available.
- **M2 — Embeddings + image (deferred).** Scope: Bedrock `InvokeModel` embeddings
  (Titan/Cohere) and image gen, as separate capability surfaces — explicitly out of
  the v1 chat path. Depends: M0. Verified by: their own mapping tests (not defined
  here). Deferred until a consumer needs them.
- **M3 — `Auth` signer generalization (Doc 29 seam).** Scope: lift credential
  resolution behind an `IdentityProvider`-style seam
  (`29-agent-security.md:200-208`) so Bedrock (SigV4) and a future Vertex (ADC)
  share one "keyless via workload identity" path; introduce the `Auth::Workload`/
  signer variant that v1 deliberately avoided. Depends: M0/M1 shipped + a second
  consumer (Vertex) on the horizon. Verified by: the seam's own E2E (Doc 29 E2E-6)
  plus Bedrock re-pointed through it with M0/M1 tests still green.
```
