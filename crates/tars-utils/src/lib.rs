//! tars-utils — pure, dependency-free helpers over tars-types.
//!
//! Charter: this crate holds ONLY stateless pure functions / algorithms
//! operating on `tars-types` values. NO I/O, NO state, NO business logic,
//! NO deps on pipeline / tools / runtime. If a helper needs to touch the
//! network, the filesystem, a clock, or mutable shared state, it does NOT
//! belong here.
//!
//! Module map:
//! - [`json_decode`] — result-side, mode-aware JSON decode
//!   (`decode` / `decode_json`; [`ResponseJsonExt`] adds `ChatResponse::json`).

pub mod json_decode;

pub use json_decode::{
    DecodeOpts, JsonAgentResponse, JsonValueType, ResponseJsonExt, TarsJsonError, decode,
    decode_json,
};
