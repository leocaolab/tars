The LlmProvider trait and every concrete backend (HTTP APIs, CLI subprocesses, Bedrock bridge) — per Doc 01, `stream` is the basic operation and `complete` aggregates it.

- Role (hex): port + adapter(HTTP: anthropic/openai/gemini/vllm; CLI subprocess: claude_cli/gemini_cli/claude_sdk/codex_cli; cassette replay; mock; bedrock bridge) — trait and impls co-located by design
- Effect budget: network (reqwest + SSE — this crate OWNS raw LLM HTTP for the workspace) | process (spawns CLI delegates; child_reaper SIGKILLs process groups on host signal) | fs (cassette record/replay, auth/key file reads) | clock (retry backoff, httpdate)
- Deps: may depend on [tars-types, tars-sandbox (SandboxPolicy for CLI delegates), tars-config (schema→instance in registry), tars-bedrock (optional, `bedrock` feature — the LEAF holds AWS logic, the `impl LlmProvider` bridge lives here), reqwest, eventsource-stream, tokio, nix(unix)]; MUST NOT import [tars-pipeline → the pipeline wraps providers, never the reverse; rusqlite → tars-storage/tars-cache/tars-melt; aws-sdk-* directly → tars-bedrock owns AWS]
- Owns concepts: [LlmProvider, LlmEventStream, ProviderRegistry, Auth/AuthResolver, HttpProviderBase/HttpAdapter, ToolCallBuffer, SchemaDialect/adapt_schema, BatchSubmitter, child_reaper, subprocess_diagnostics, every concrete *Provider + *Builder]
- Reason to change (the ONE): a backend's wire/subprocess contract changes, or a new backend is added behind the same trait
- Belongs here: a new HTTP backend adapter; an SSE quirk fix; CLI-delegate spawn/reap logic
- Does NOT belong: retry/cache/telemetry/budget policy → tars-pipeline middleware; AWS Converse mapping → tars-bedrock; sandbox profile construction → tars-sandbox (this crate only THREADS a SandboxPolicy through); model/routing declarations → tars-config

Note: port + many adapters in one crate is this crate's stated design (Doc 01), not drift — the boundary to police is inward (no pipeline knowledge) and downward (AWS stays in tars-bedrock).
