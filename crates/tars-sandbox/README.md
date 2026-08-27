OS exec-confinement mechanism (macOS Seatbelt / Linux bubblewrap) as a pure policy→argv builder — it never spawns; it sits below both spawners (tars-tools and tars-provider) so neither owns the other's confinement.

- Role (hex): core (pure mechanism; deliberately zero deps)
- Effect budget: none (builds `(program, argv)` / profile strings; the CALLER spawns; fail-closed on unbuildable policy)
- Deps: may depend on []; MUST NOT import [anything — zero-dep is the design; std::process::Command spawning → the callers (tars-tools BashTool, tars-provider claude_cli); tars-types → this crate sits BELOW tars-types (tars-types depends on it)]
- Owns concepts: [SandboxMode (ReadOnly / WorkspaceWrite / DangerFullAccess), SandboxPolicy, SandboxPolicy::wrap, seatbelt_profile, bwrap_argv, default_tmp_writable_roots]
- Reason to change (the ONE): the confinement model changes (a new OS backend, a new jail dimension)
- Belongs here: a Landlock/Linux-namespace profile builder; a new writable-root rule; profile-string generation tests
- Does NOT belong: actually spawning a process → tars-tools (BashTool) or tars-provider (CLI backends); sandbox *configuration* schema / TOML → tars-config (`SandboxConfig`, `resolve_policy`); deciding WHEN to sandbox → the calling tool/provider policy
