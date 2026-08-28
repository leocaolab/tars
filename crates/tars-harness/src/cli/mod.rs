//! `tars` — the command line, and the arg definitions behind each command.
//!
//! The binary is `src/bin/tars.rs`; it does nothing but parse and dispatch into
//! these modules. They live in this crate rather than a crate of their own
//! because a separate binary crate bought one thing — a `Cargo.toml` — and cost
//! a boundary that every command had to be threaded across: the harness's own
//! flags sat on one side of it while the machinery they drive sat on the other.
pub mod bench;
pub mod config_loader;
pub mod dispatch;
pub mod event_store;
pub mod events;
pub mod harness;
pub mod init;
pub mod model_library;
pub mod model_query;
pub mod models;
pub mod probe;
pub mod providers_cmd;
pub mod run_report;
pub mod trajectory;
