The `tars` binary — clap routing + exit-code translation in main.rs, per-subcommand modules doing the work; the workspace's composition root that wires config → providers → pipeline → runtime → stores.

- Role (hex): composition-root + shell (CLI driving adapter)
- Effect budget: fs (config + `$TARS_HOME/.env` via dotenvy, event-store paths) | network (BOTH via the provider layer for `tars run` AND raw reqwest for `tars models`/`tars providers` live model-list queries — see friction) | process (signal handling: Ctrl-C → CancellationToken) | clock (chrono timestamp formatting)
- Deps: may depend on [every tars-* layer it composes: types, utils, config, provider, pipeline, cache, melt, storage, runtime, tools; clap, anyhow, reqwest, dotenvy, chrono]; MUST NOT import [rusqlite → open stores through tars-storage/tars-melt helpers; axum → tars-server is the HTTP shell]
- Owns concepts: [the `tars` command surface (run, plan, eval, bench, events, models, providers, init, probe, trajectory, …) and its exit-code / output-formatting contract — no domain types; everything domain lives below]
- Reason to change (the ONE): the CLI surface changes (a subcommand, a flag, an output format)
- Belongs here: a new subcommand that composes existing layers; clap arg parsing; human-facing rendering of lower-layer results
- Does NOT belong: any reusable domain logic grown inside a subcommand module → extract to the owning layer; a new middleware → tars-pipeline; provider wire handling → tars-provider

Friction (flagged): `tars models` / `tars providers` build their own `reqwest::Client` (src/models.rs:121-122, src/model_query.rs:233, src/providers_cmd.rs:124) and hit provider REST endpoints directly — a deliberate bypass (Cargo.toml documents it: list-models isn't a chat operation and the LlmProvider port has no such method), but it means provider-API knowledge (URLs, auth headers) now lives in TWO layers. If the port ever grows a `list_models`, this code must move to tars-provider.
