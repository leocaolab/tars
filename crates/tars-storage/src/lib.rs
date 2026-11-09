//! Keeps the durable store and opens the SQLite files the stores are built from.
//!
//! [`durable`] is the durable store: the task queue and each task's trajectory, with its schema
//! and migrations here. It decides whether a write is admitted, never what runs next or when.
//! The other stores (the event logs, the tap store) live in their own crates with their own
//! migrations and build on [`Db`]. [`durable::journal`] is the port the durable store seals its
//! journals through; the files' format belongs to whoever implements it.

#![deny(clippy::print_stdout, clippy::print_stderr)]

mod db;
pub mod durable;

pub use db::{Access, Db, DbError, Location, is_lock_contention};
