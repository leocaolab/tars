//! `tars-bedrock` — AWS Bedrock as a first-class [`LlmProvider`] via the
//! unified **Converse** API, keyless (AWS credential chain → SigV4 signed
//! by the SDK). See `docs/architecture/31-bedrock.md`.
//!
//! Feature-gated behind `tars-provider`'s `bedrock` feature and kept out
//! of `default-members`, so the heavy AWS SDK subtree never enters a build
//! that doesn't ask for Bedrock (Doc 31 §4).
//!
//! Module map:
//! - [`mapping`]  — pure `ChatRequest` ↔ Converse converters (no I/O)
//! - [`document`] — `serde_json::Value` ↔ `aws_smithy_types::Document`
//! - [`error`]    — SDK error → typed [`tars_types::ProviderError`]
//! - [`stream`]   — pure `ConverseStream` event → canonical `ChatEvent`
//! - [`client`]   — keyless lazy SigV4 client; aggregate `complete_response`
//!   plus the incremental `stream_response` (M1)
//!
//! This crate holds only the AWS-specific logic and returns canonical
//! `tars-types` values; it does **not** depend on `tars-provider`. The
//! thin `impl LlmProvider` that adapts [`BedrockClient`] to the provider
//! trait lives in `tars-provider` behind its `bedrock` feature, keeping
//! the crate graph acyclic (Doc 31 §4; see [`client`] docs).
//!
//! M0 was non-streaming (`converse()`); M1 adds true incremental
//! `ConverseStream` (`client::stream_response` + [`stream`]).

pub mod client;
pub mod document;
pub mod error;
pub mod mapping;
pub mod stream;

pub use client::{BedrockClient, BedrockEventStream, default_capabilities};
pub use mapping::converse_output_to_response;
pub use stream::StreamTranslator;
