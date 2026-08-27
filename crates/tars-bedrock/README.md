AWS Bedrock as an LLM backend via the unified Converse API — keyless (credential chain → SigV4), holding ONLY the AWS-specific logic and returning canonical tars-types values.

- Role (hex): adapter (AWS Bedrock backend; a LEAF crate — the `impl LlmProvider` bridge lives in tars-provider behind its `bedrock` feature to keep the graph acyclic)
- Effect budget: network (AWS SDK Converse / ConverseStream calls; credential-chain resolution)
- Deps: may depend on [tars-types, aws-config, aws-sdk-bedrockruntime, aws-smithy-types, tokio, futures, async-stream]; MUST NOT import [tars-provider — would cycle (Cargo.toml documents the ban); reqwest → the AWS SDK is the HTTP stack here, tars-provider owns generic HTTP LLM adapters; rusqlite → tars-storage/tars-cache/tars-melt]
- Owns concepts: [BedrockClient, BedrockEventStream, StreamTranslator, converse_output_to_response, ChatRequest↔Converse mapping, Value↔Document conversion, SDK-error→ProviderError]
- Reason to change (the ONE): the Bedrock/Converse wire contract changes (new event type, new model family quirk, SDK upgrade)
- Belongs here: a Converse content-block mapping; a new Bedrock stream-event translation; SDK error classification
- Does NOT belong: the `impl LlmProvider for …` bridge → tars-provider `backends::bedrock`; retry/cache/telemetry policy → tars-pipeline middleware; another cloud's SDK adapter → its own leaf crate mirroring this one
