Installs the process's `tracing` subscriber — pretty or JSON on stderr, an `EnvFilter`, and opt-in OTLP trace and metric export. It stores nothing and reads nothing back.

- Role (hex): adapter(tracing subscriber / OTLP exporter), called once by a binary's composition root
- Effect budget: network (OTLP gRPC span/metric export — `otlp` feature only, off by default) | (stderr log emission via tracing-subscriber) | process-global state (the global `tracing` dispatcher and `log` logger, set once by `init`)
- Deps: may depend on [tars-types (env:: reads only), tracing, tracing-subscriber, tracing-log, opentelemetry* (`otlp` feature)]; MUST NOT import [tars-storage / tars-melt / any store → telemetry is write-only and holds no record; tars-llm and every emitting crate → they emit through the `tracing` macros and never know which subscriber is installed]
- Owns concepts: [TelemetryConfig::from_env → EnvConfig { config, warnings } (the one place the `TARS_` / `OTEL_` / `RUST_LOG` variables are read; `NO_COLOR` and whether stderr is a terminal are `init`'s, which installs the writer they are about), init → Installed { guard, warnings }, TelemetryConfig, SampleRatio (a ratio in 0..=1, made only by `new`), TelemetryFormat, TelemetryGuard, TelemetryError, TelemetryWarning (a setting that was set but skipped, returned to the caller, never printed)]
- Reason to change (the ONE): how a tars binary formats, filters or exports its telemetry
- Belongs here: an EnvFilter/format rule; the OTel layer composition; a new exported metric
- Does NOT belong: the event log or any stored record → tars-melt; emitting an event → the crate the event is about, through `tracing`; telling the operator about a skipped setting → the binary (`init` returns the warnings, never prints them)

Used by: tars-cli (the `tars` binary), the process tars starts. A library crate does not depend on it.
