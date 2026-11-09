//! Holds the models and logs that record a run's calls and events ([`event`]), and opens a
//! durable store file with its sealed journals in Parquet beside it (`durable_file`, with the
//! `parquet` feature). Installing the process's `tracing` subscriber and exporting telemetry is
//! tars-telemetry's; this crate only emits through the `tracing` macros.

#![deny(clippy::print_stdout, clippy::print_stderr)]

#[cfg(feature = "parquet")]
pub mod durable_file;
pub mod event;
