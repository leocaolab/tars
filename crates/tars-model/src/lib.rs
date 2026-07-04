//! `tars-model` — the domain model: the **Agent** abstraction + its
//! vocabulary. Pure contracts, zero implementation.
//!
//! ## Why this crate exists
//!
//! The Agent abstraction used to be buried in `tars-runtime` (an
//! implementation crate). That's the "abstraction in the implementation"
//! smell — and it's exactly how the contract kept leaking the LLM
//! implementation (a `ChatRequest` on the agent interface). This crate is
//! the foundational home for the abstractions, and its dependency boundary
//! ENFORCES the discipline:
//!
//! > `tars-model` depends ONLY on `tars-types` (primitives). It may NOT
//! > depend on `tars-pipeline` (`LlmService`) or `tars-tools`
//! > (`ToolRegistry`). So the `Agent` trait physically cannot reference the
//! > LLM machinery — a user agent that uses no LLM stays first-class.
//!
//! ```text
//! tars-types  (primitives)
//!    ↑
//! tars-model  (Agent / Task / Skill / Permission — THIS crate)
//!    ↑
//! tars-pipeline · tars-tools   (lower-concern implementations)
//!    ↑
//! tars-runtime  (NATIVE agent: Session-based; impls `Agent`)
//! ```
//!
//! ## The model in one breath
//!
//! An [`Agent`] is a [`SkillSet`] you hand a [`Task`] to ([`Agent::run`]).
//! How it runs (an LLM [`AgentContext`]-scoped Session loop, a subprocess,
//! a human) is an implementation detail. [`Permissions`] gate what it's
//! allowed to do; the recursive [`Task`] is the unit of intent. See
//! `docs/architecture/20-agent-abstraction.md`.

mod agent;
mod context;
mod ids;
mod permission;
mod role;
mod skill;
mod task;

pub use agent::{Agent, TaskError, AgentOutput};
pub use context::AgentContext;
pub use ids::{AgentId, TaskId};
pub use permission::{Decision, Permissions};
pub use role::AgentRole;
pub use skill::{Skill, SkillSet};
pub use task::{Task, TaskInput};
