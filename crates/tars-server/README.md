Personal-mode HTTP/REST server over the tars pipeline (Doc 12, M6 subset) — a thin axum shell that makes the already-built pipeline curl-able; deliberately no auth/multi-tenant, loopback-bind by default.

- Role (hex): shell (HTTP driving adapter + the `tars-server` bin)
- Effect budget: network (axum TcpListener, SSE streaming; loopback by default, loud warning otherwise) | process-signals (Ctrl-C shutdown)
- Deps: may depend on [tars-pipeline, tars-provider (registry construction), tars-cache, tars-config, tars-melt (telemetry init), tars-types, axum, tokio]; MUST NOT import [rusqlite → the three store owners; reqwest → it SERVES HTTP, providers make outbound LLM HTTP; tars-runtime → this server exposes completion, not the agent loop (adding trajectories here would be a new endpoint surface owned elsewhere first)]
- Owns concepts: [AppState, router, the /healthz, /v1/providers, /v1/complete, /v1/complete/stream HTTP contract and its request/response DTOs]
- Reason to change (the ONE): the HTTP surface changes (an endpoint, a DTO, a bind/serve behavior)
- Belongs here: a new endpoint over an existing pipeline capability; SSE keep-alive tuning; request→ChatRequest DTO mapping
- Does NOT belong: middleware/policy logic → tars-pipeline; auth/IAM/tenancy → the future M6 tars-security integration (explicitly out of scope here, lib.rs says so); provider construction rules → tars-provider registry + tars-config
