Pure, dependency-free helpers over tars-types — stateless algorithms only; no I/O, no state, no business logic.

- Role (hex): core (pure function library)
- Effect budget: none (charter-enforced: no network, no fs, no clock, no shared mutable state)
- Deps: may depend on [tars-types, serde, serde_json, thiserror]; MUST NOT import [tokio/reqwest/rusqlite → any crate that needs an effect is by definition not tars-utils; tars-pipeline/tars-tools/tars-runtime → helpers must not know upper layers]
- Owns concepts: [json_decode (decode, decode_json, DecodeOpts, JsonAgentResponse, JsonValueType, TarsJsonError, ResponseJsonExt)]
- Reason to change (the ONE): a new pure algorithm over tars-types values is needed by ≥2 consumers
- Belongs here: mode-aware JSON extraction from a ChatResponse; a pure text/schema normalization; a stateless retry-jitter formula
- Does NOT belong: anything async or effectful → the owning layer (provider/pipeline/tools); a helper used by exactly one crate → keep it private in that crate; a new shared *type* → tars-types
