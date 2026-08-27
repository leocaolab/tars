//! LLM / provider / pipeline types — the shapes a call is made of.
//!
//! Everything here is re-exported flat from the crate root, which is how every
//! consumer names it; the module path exists so a reader can see the grouping.

pub mod auth;
pub mod batch;
pub mod cache;
pub mod chat;
pub mod context; // RequestContext — the LLM-call one
pub mod error;
pub mod events;
pub mod http_extras;
pub mod model;
pub mod provider_profile;
pub mod response;
pub mod schema;
pub mod tools;
pub mod usage;
