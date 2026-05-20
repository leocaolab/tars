# Anthropic HTTP API provider — Builder API guide

The `anthropic` provider talks directly to Anthropic's HTTP API
(`https://api.anthropic.com/v1/messages` and friends). You bring an API
key; the library does HTTP / streaming / retries / tool-call
normalization / prompt cache markers.

If you want the subscription-authenticated path through your local
`claude` binary instead, see [`claude-cli.md`](./claude-cli.md). Both
implement the same `LlmProvider` trait — swapping is a one-line change
at the Builder site, by design.

---

## 1. When to pick this over `claude_cli`

| Situation | Pick |
|---|---|
| Production server, no interactive login, you have an API key | **`anthropic`** |
| You're billing through Anthropic Console / want per-call cost telemetry from `usage` that actually means what it says | **`anthropic`** |
| You're on Claude Pro/Max subscription, no API key, dev/local use | `claude_cli` |
| You need real OpenAI-style streaming with tool use mid-flight | **`anthropic`** (CLI streaming is JSONL-framed, different shape) |
| You want the Claude Code agent runtime (file ops, bash) wrapped in a `Provider` | `claude_cli` with opt-in flags |

The `anthropic` provider is a pure inference channel by default and by
construction — there's no agent loop, no auto-memory, no `CLAUDE.md`
auto-discovery. Token counts reported via `usage.input_tokens` /
`usage.output_tokens` are the actual model billing numbers, not a CLI
accumulator (cf. [`claude-cli.md §1`](./claude-cli.md#1-the-surprise-claude_cli-is-not-a-pure-inference-channel-by-default)
for why this matters).

---

## 2. Builder API

```rust
pub struct AnthropicProviderBuilder { /* … */ }

impl AnthropicProviderBuilder {
    pub fn new(id: impl Into<ProviderId>, auth: Auth) -> Self;

    pub fn base_url(self, url: impl Into<String>) -> Self;       // default: api.anthropic.com
    pub fn api_version(self, v: impl Into<String>) -> Self;      // default: 2023-06-01
    pub fn capabilities(self, c: Capabilities) -> Self;          // override defaults
    pub fn extras(self, e: HttpProviderExtras) -> Self;          // headers / query params

    pub fn build(
        self,
        http: Arc<HttpProviderBase>,
        auth_resolver: Arc<dyn AuthResolver>,
    ) -> Arc<AnthropicProvider>;
}
```

### The asymmetry with `claude_cli` — `build()` needs two collaborators

`ClaudeCliProviderBuilder::build()` takes nothing because subprocess
auth is delegated to the OS-installed `claude` binary. The HTTP API
backend can't do that: it actually needs to issue HTTP requests with
your credential. So `.build()` requires:

- **`Arc<HttpProviderBase>`** — a shared `reqwest`-backed HTTP client
  with connection pooling, retry-aware timeouts, and the standard
  middleware hooks. You construct it once per process, share across
  providers.
- **`Arc<dyn AuthResolver>`** — looks up the actual credential at call
  time. For env-var auth this is just `BasicAuthResolver::new()`; for
  production secret managers (Vault, GCP Secret Manager, etc.) you
  plug in a custom impl.

### Minimal call site

```rust
use std::sync::Arc;
use tars_provider::{
    AnthropicProviderBuilder, BasicAuthResolver, HttpProviderBase, HttpProviderConfig,
};
use tars_types::Auth;

let http = Arc::new(HttpProviderBase::new(HttpProviderConfig::default())?);
let auth_resolver = Arc::new(BasicAuthResolver::new());

let provider = AnthropicProviderBuilder::new("claude_api", Auth::env("ANTHROPIC_API_KEY"))
    .build(http, auth_resolver);
```

This is verbose for one provider. The pattern pays off when you have
several HTTP-API providers — they share the same `http` and
`auth_resolver`. **In practice, most users let
`ProviderRegistry::from_config` do this construction** (§4).

### Overriding base URL — proxies, mocks, regional endpoints

```rust
let provider = AnthropicProviderBuilder::new("claude_api", auth)
    .base_url("https://my-anthropic-proxy.internal:8443")
    .build(http, auth_resolver);
```

Use for:
- Routing through a corporate proxy with TLS interception
- Hitting a recording proxy (e.g. [`vcrpy`](https://github.com/kevin1024/vcrpy)-style
  HTTP cassettes) during tests
- Pointing at a regional Anthropic endpoint if/when those ship

### Overriding `api_version` — pinning for stability

The Anthropic `anthropic-version` header changes occasionally to gate
new features. Pin to a known-good version if you've validated against
a specific one:

```rust
let provider = AnthropicProviderBuilder::new("claude_api", auth)
    .api_version("2023-06-01")
    .build(http, auth_resolver);
```

Default is whatever the library was built against. Override sparingly.

### Capabilities — telling the rest of tars what this model can do

Capabilities is the metadata layer that the pre-flight + middleware use
to refuse incompatible requests (asking for vision on a text-only
model, asking for parallel tool calls on a model that doesn't support
them, etc.). Defaults match modern Sonnet/Opus:

```rust
Capabilities {
    max_context_tokens: 200_000,
    max_output_tokens: 8_192,
    supports_tool_use: true,
    supports_parallel_tool_calls: true,
    supports_structured_output: StructuredOutputMode::ToolUseEmulation,
    supports_vision: true,
    supports_thinking: true,
    supports_cancel: true,
    prompt_cache: PromptCacheKind::ExplicitMarker,
    streaming: true,
    modalities_in:  { Text, Image },
    modalities_out: { Text },
    pricing: Pricing::default(),
}
```

Override when:
- You're using a smaller model (Haiku) with a smaller context window
- You want to force-disable tool use because your downstream code can't
  handle it
- You've measured real pricing and want budget middleware to use it

```rust
use tars_types::Capabilities;
let caps = Capabilities { max_output_tokens: 4096, supports_thinking: false, ..Default::default() };
let provider = AnthropicProviderBuilder::new("claude_api", auth)
    .capabilities(caps)
    .build(http, auth_resolver);
```

### Extras — extra HTTP headers / query params

`HttpProviderExtras` lets you inject static and env-resolved headers
(`http_headers`, `env_http_headers`) and query params. Use for:

- Cloudflare-style auth headers (`CF-Access-Client-Id`,
  `CF-Access-Client-Secret`)
- Vendor-specific feature flags (`anthropic-beta: prompt-caching-2024-07-31`)
- Tracing headers (`x-trace-id`) forwarded from upstream

```rust
let extras = HttpProviderExtras {
    http_headers: vec![("anthropic-beta".into(), "experimental-x".into())],
    env_http_headers: vec![("X-Org-Id".into(), "ORG_ID".into())],  // value from env
    query_params: vec![],
};
let provider = AnthropicProviderBuilder::new("claude_api", auth)
    .extras(extras)
    .build(http, auth_resolver);
```

---

## 3. Authentication

`Auth` is a serde-tagged enum from `tars-types`. Three variants, plus
sugar:

```rust
pub enum Auth {
    None,                                // for the local-server backends; not valid here
    Delegate,                            // for CLI backends; not valid here
    Secret { secret: SecretRef },        // what you'll use
}

// Sugar:
Auth::env("ANTHROPIC_API_KEY")           // → Secret { SecretRef::Env { var: "..." } }
Auth::inline("sk-ant-...")               // → Secret { SecretRef::Inline { value: "..." } }  TEST/DEV ONLY
Auth::file("/run/secrets/anthropic.key") // → Secret { SecretRef::File { path: "..." } }
```

### `Auth::env` — the common case

```rust
let auth = Auth::env("ANTHROPIC_API_KEY");
```

Resolved at call time via `BasicAuthResolver` reading `std::env`.

### `Auth::file` — Kubernetes-style mounted secrets

```rust
let auth = Auth::file("/run/secrets/anthropic.key");
```

The resolver reads the file each time (with caching depending on the
resolver impl). Works with Kubernetes secret mounts, Docker secrets,
HashiCorp Vault file sinks.

### `Auth::inline` — **test / dev only**

```rust
let auth = Auth::inline("sk-ant-...");
```

Don't put this in production code or check it into git. The pre-flight
warns if it sees a non-passive Auth::inline used outside test contexts.

### Custom secret managers (Vault, GCP Secret Manager, AWS Secrets Manager)

Implement `AuthResolver` yourself and pass it to `.build(http, auth_resolver)`:

```rust
struct VaultResolver { /* … */ }

#[async_trait]
impl AuthResolver for VaultResolver {
    async fn resolve(&self, auth: &Auth, ctx: &RequestContext)
        -> Result<ResolvedAuth, AuthError>
    {
        // look up auth using ctx.tenant_id for namespacing
    }
}
```

The `ctx: &RequestContext` argument is the hook for per-tenant secret
namespacing. Production multi-tenant deployments use this; single-tenant
uses `BasicAuthResolver` and ignores `ctx`.

`ResolvedAuth` deliberately implements `Debug` with credential
redaction — `tracing::error!(auth = ?auth)` is safe. Audit ref:
`tars-provider-src-auth-2`.

---

## 4. TOML configuration — the path most users take

```toml
[providers.claude_api]
type = "anthropic"
default_model = "claude-opus-4-7"
auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY" } }

# All optional — values shown are defaults.
# base_url = "https://api.anthropic.com"
# api_version = "2023-06-01"

# Optional HTTP extras (flattened into the variant body).
# http_headers = [["anthropic-beta", "prompt-caching-2024-07-31"]]
# env_http_headers = [["X-Org-Id", "ORG_ID"]]
# query_params = []
```

`ProviderRegistry::from_config` constructs the `HttpProviderBase` and
`BasicAuthResolver` automatically; you get back a registry with the
provider already built. Most users never touch `AnthropicProviderBuilder`
directly.

```rust
use tars_provider::ProviderRegistry;

let registry = ProviderRegistry::from_config(&cfg, /* ... */)?;
let provider = registry.get(&ProviderId::new("claude_api")).unwrap();
```

### Auth shapes in TOML

```toml
# env var
auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY" } }

# file
auth = { kind = "secret", secret = { source = "file", path = "/run/secrets/anthropic.key" } }

# inline (DON'T)
auth = { kind = "secret", secret = { source = "inline", value = "sk-ant-..." } }
```

### Multiple Anthropic providers

You can name as many `anthropic`-type providers as you want — they'll
share the `HttpProviderBase` connection pool automatically:

```toml
[providers.claude_prod]
type = "anthropic"
default_model = "claude-opus-4-7"
auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY_PROD" } }

[providers.claude_staging]
type = "anthropic"
default_model = "claude-sonnet-4-5"
auth = { kind = "secret", secret = { source = "env", var = "ANTHROPIC_API_KEY_STAGING" } }
base_url = "https://anthropic-proxy.internal"
```

---

## 5. Composing with the Pipeline

Identical to `claude_cli` — Pipeline accepts any `Arc<dyn LlmProvider>`.
See [`claude-cli.md §5`](./claude-cli.md#5-composing-a-builder-built-provider-with-the-pipeline)
for the three patterns (direct, Registry, `map_providers`).

The one thing worth repeating: middleware does not see `Auth`,
`base_url`, `api_version`, or any HTTP detail. Those are entirely
contained inside the provider impl, behind the `LlmProvider` trait
contract. Telemetry sees the request shape, the result shape, and the
latency — nothing more. Don't try to plumb HTTP-specific concerns into
middleware; if you find yourself wanting to, the right answer is
usually `extras` (§2) or a custom `AuthResolver` (§3).

---

## 6. Defaults at a glance

| Knob | Default |
|---|---|
| `base_url` | `https://api.anthropic.com` |
| `api_version` | `2023-06-01` (the value the library was built against) |
| `default_model` (config builtin) | `claude-opus-4-7` |
| `max_context_tokens` | `200_000` |
| `max_output_tokens` | `8_192` |
| `supports_tool_use` | `true` |
| `supports_thinking` | `true` |
| `streaming` | `true` |
| `prompt_cache` | `ExplicitMarker` |

Override any of these via the Builder methods (§2) or via TOML (§4).

---

## 7. Anti-patterns

A short list of things that look reasonable and aren't:

1. **Don't hand-construct `HttpProviderBase` per provider.** It owns a
   connection pool. Share one process-wide.
2. **Don't put `Auth::inline` in production config.** Use env or file.
   The pre-flight will warn.
3. **Don't override `Capabilities` to "lie" about a model's
   capabilities to skirt validation.** The capability layer exists to
   refuse incompatible requests before they hit the network. Lying
   means the network call fails with a less informative error.
4. **Don't override `api_version` "just to be safe."** The default
   tracks what the SDK was tested against. Pinning is fine; rolling
   forward without testing is not.
5. **Don't reach for the Builder when TOML + Registry would do.**
   80% of users should use `ProviderRegistry::from_config`. The
   Builder is for: tests, embedded library use, surgical overrides via
   `map_providers`.

---

## 8. See also

- [`claude-cli.md`](./claude-cli.md) — subscription-authenticated path
  with the agent-loop discussion
- [`../architecture/01-llm-provider.md`](../architecture/01-llm-provider.md) —
  full provider-trait spec; HTTP-API and CLI backends share §10 (cache),
  §11 (errors), §12 (routing), §15 (anti-patterns)
- [`../architecture/06-config-multitenancy.md`](../architecture/06-config-multitenancy.md) —
  per-tenant secret namespacing via custom `AuthResolver`
