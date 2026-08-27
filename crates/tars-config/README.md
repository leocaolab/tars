Declarative configuration for TARS (Doc 06) — the schema + loader/validator for the global immutable `~/.tars` config; it declares providers but never instantiates them.

- Role (hex): core (config schema) + adapter(fs — TOML/figment loading)
- Effect budget: fs (read config files from `~/.tars` / explicit paths; env-var reads for key injection)
- Deps: may depend on [tars-types, tars-sandbox (SandboxConfig → SandboxPolicy resolution), sisurf-core (SearchConfig — schema OWNED by sisurf, deserialized not redeclared), toml, figment, dirs]; MUST NOT import [tars-provider — instantiation lives in `ProviderRegistry::from_config()` which depends on US, not the reverse (lib.rs states the ban); reqwest → no network at config time; rusqlite → tars-storage/tars-cache/tars-melt]
- Owns concepts: [Config, ConfigManager, ConfigError, ProvidersConfig, ProviderConfig, RoleConfig, RoutingConfig, SandboxConfig/SandboxModeConfig/resolve_policy, model_kb, builtin provider defaults + merge, resolve_home, default_config_path, web_search key injection]
- Reason to change (the ONE): the user-facing configuration SCHEMA changes (a new declarable knob)
- Belongs here: a new `[providers.X]` field + its validation; a builtin default; a path-resolution rule
- Does NOT belong: building an OpenAiProvider from a ProviderConfig → tars-provider registry; runtime hot-reload/tenant overlays → deliberately deleted (Doc 06 deprecated appendix), do not reintroduce; secrets FETCHING (vault/network) → a future secret-manager adapter, not the schema crate

Friction (known, accepted): the external `sisurf-core` git dep enters the config layer purely to reuse `SearchConfig` — schema single-ownership was judged worth the heavyweight edge (Cargo.toml documents it).
