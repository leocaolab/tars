//! Holds stateless functions over `tars-types` values: no IO, no clock, no shared state,
//! and no dependency on the pipeline, tools or runtime. A helper that needs any of those
//! belongs to the layer that has it.

#![deny(clippy::print_stdout, clippy::print_stderr)]

mod error_chain;
pub mod json_decode;

pub use error_chain::display_chain;
pub use json_decode::{
    DecodeJsonError, DecodeOpts, JsonAgentResponse, JsonValueType, ResponseJsonExt, decode,
    decode_json, find_json_array_start, salvage_json_array,
};
