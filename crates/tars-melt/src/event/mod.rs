//! The logs a run's calls are recorded in. [`call_log`] writes one `llm_call` row per call, both
//! bodies inline, and one `eval_score` row per score, through the sealed-batch [`log`]
//! mechanism; [`wire`] holds what a provider's transport sent and received, one row per attempt.
//! [`event_logs::EventLogs::open_in_dir`] opens a directory's logs.
//!
//! A run's trajectory is not here: the durable store owns it, and [`log`] only seals the journal
//! rows the durable store hands it into files.

pub mod call_log;
#[cfg(feature = "parquet")]
pub mod event_logs;
pub mod log;
pub mod session_transcript;
pub mod wire;
