# AWS Bedrock provider — keyless, IAM/SigV4 guide

The `bedrock` provider gives tars **keyless, IAM/SigV4-authenticated**
access to Bedrock-hosted models — Claude, Nova, Llama, Mistral, Cohere,
… — through Bedrock's unified **`Converse` / `ConverseStream`** API. You
bring **no API key**: credential resolution and request signing are
delegated wholesale to the AWS SDK. This is the production/cloud path —
on AWS the workload's own identity (IRSA / EC2 instance role / ECS task
role / SSO) signs every request, so there is no key material at rest.

The full design is [`architecture/31-bedrock.md`](../architecture/31-bedrock.md);
this is the user-facing walkthrough.

---

## 1. The two things that make Bedrock different

Every other provider guide starts with "bring an API key." Bedrock
breaks that in two structural ways, and both are deliberate:

1. **It's keyless.** There is intentionally **no `auth` field** on the
   `bedrock` config block. tars hand-rolls no SigV4 and reads no secret
   — the AWS SDK's default credential chain resolves the caller's
   identity and signs every request. Contrast the
   [`anthropic`](./anthropic.md) / OpenAI / Gemini backends, which
   *require* a key and reject `Auth::None`; Bedrock's config validation
   deliberately omits that rejection.

2. **It's feature-gated.** The AWS SDK (`aws-sdk-bedrockruntime` +
   `aws-config` + the smithy runtime) is a heavy dependency subtree, so
   it is **absent from the default build**. You must compile tars with
   `--features bedrock`. Without it, a declared `bedrock` provider fails
   at registry-build time with an actionable `FeatureDisabled` error
   ("rebuild with `--features bedrock`") rather than being silently
   dropped.

One mapping (`ChatRequest` ↔ Converse) covers **all** Bedrock model
families, because Converse is Bedrock's own cross-model normalization
layer — tars does not write per-model request bodies and does not reuse
the Anthropic Messages mapping.

---

## 2. Building with the `bedrock` feature

```
cargo build   --features bedrock
cargo run     --features bedrock -- ...
```

`tars-bedrock` is a separate workspace crate kept out of
`default-members`, so a normal `cargo build --workspace` stays fast and
the AWS SDK never enters the default dep graph. The feature on
`tars-provider` (`bedrock = ["dep:tars-bedrock"]`) is what pulls it in
and lights up the registry's Bedrock build arm.

---

## 3. Authentication — the AWS credential chain

There is no credential in tars config. On the first call the provider
lazily builds an AWS SDK client via `aws_config::defaults(...).load()`,
which walks the standard AWS precedence:

```
env vars (AWS_ACCESS_KEY_ID / AWS_SESSION_TOKEN / …)
  → AWS_PROFILE / ~/.aws/config + ~/.aws/credentials
  → SSO
  → ECS / EKS container credentials
  → EC2 IMDS instance role
```

Whatever that chain resolves signs the `Converse` call with SigV4. The
optional `profile` field just names an `~/.aws` profile for the **laptop
case**; on AWS you omit it and the ambient workload role (IRSA /
instance / task role) wins with **zero config change** — the same
`[providers.…]` block is portable from your laptop to a pod.

The client is built **once per provider** (a `OnceCell`), lazily on the
first request, so a misconfigured region or unresolvable identity
surfaces as a classified `ProviderError` on the first call — never a
silent unsigned request. `OnceCell` doesn't cache a *failed* init, so a
transient credential-endpoint blip can be retried on the next call.

### IAM permissions

The signing identity needs Bedrock invoke permissions on the model(s)
you call. Minimally:

```json
{
  "Effect": "Allow",
  "Action": [
    "bedrock:InvokeModel",
    "bedrock:InvokeModelWithResponseStream"
  ],
  "Resource": "arn:aws:bedrock:us-east-1::foundation-model/us.anthropic.claude-sonnet-4-5-v1:0"
}
```

`InvokeModel` covers the non-streaming `converse()` path;
`InvokeModelWithResponseStream` covers `converse_stream()`. Model access
must also be enabled for your account/region in the Bedrock console —
a model you haven't enabled comes back as a `ValidationException`,
surfaced (message intact) as `InvalidRequest`.

---

## 4. TOML configuration

```toml
[providers.claude_bedrock]
type   = "bedrock"
region = "us-east-1"
model  = "us.anthropic.claude-sonnet-4-5-v1:0"

# No `auth` field — keyless by design (the AWS credential chain signs).
# `profile` is optional and laptop-only; omit it on AWS so the ambient
# workload role (IRSA / instance / task role) is used instead.
# profile = "dev"
```

`region` and `model` are the only required fields, both validated
non-empty at config time. Note the field is named `model` (not
`default_model` like the other providers) — that mirrors Doc 31 and is
what `default_model()` reports for tier resolution. `model` is Bedrock's
model id, e.g. `us.anthropic.claude-sonnet-4-5-v1:0`,
`amazon.nova-pro-v1:0`, a Llama or Mistral id — any family Converse
supports.

---

## 5. Capabilities and behavior

The provider ships conservative default capabilities suited to the
common Claude/Nova chat case:

| Knob | Default |
|---|---|
| `max_context_tokens` | `200_000` |
| `max_output_tokens` | `8_192` |
| `supports_tool_use` | `true` |
| `supports_parallel_tool_calls` | `true` |
| `supports_vision` | `true` |
| `supports_thinking` | `false` (M0 doesn't yet translate the reasoning knob into per-model `additionalModelRequestFields`) |
| `streaming` | `true` |
| modalities in | Text, Image |

The non-streaming path (`complete()`) uses the unary `converse()` call,
which is strictly cheaper than streaming for the aggregate case;
streaming uses `ConverseStream`, mapping the SDK's typed event union
onto the same `ChatEvent` contract every other provider emits (tool-call
args assemble across deltas via the shared `ToolCallBuffer`).

### Errors carry the truth

AWS SDK errors are classified into typed `ProviderError`s that **carry
the service's own message** (no sentinel):

| AWS exception | `ProviderError` |
|---|---|
| `AccessDeniedException` / `UnauthorizedException` | `Auth` |
| `ThrottlingException` | `RateLimited` |
| `ModelNotReadyException` / service unavailable | `ModelOverloaded` |
| `ValidationException` (incl. model-not-enabled) | `InvalidRequest` |
| other | `Internal` |

So a "you don't have access to the model with the specified model ID"
comes back as an `InvalidRequest` with that exact message, not a
`parse_failed`-style token.

---

## 6. See also

- [`architecture/31-bedrock.md`](../architecture/31-bedrock.md) — the
  full design: Converse-vs-InvokeModel decision, the keyless auth
  stance, the crate boundary, streaming, and the M3 `Auth`-signer
  generalization
- [`anthropic.md`](./anthropic.md) — Claude via Anthropic's own HTTP
  API (the key-based, non-Bedrock path); routing picks Claude-direct
  vs Claude-on-Bedrock purely by provider id
- Implementation: `crates/tars-bedrock/**` (AWS-specific logic, a leaf
  crate depending only on `tars-types`) + the
  `impl LlmProvider` bridge in
  `crates/tars-provider/src/backends/bedrock.rs`; config variant
  `ProviderConfig::Bedrock` in `crates/tars-config/src/providers.rs`
