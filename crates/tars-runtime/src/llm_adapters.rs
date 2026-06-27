//! [`Worker`] and [`Critic`] impls backed by the existing LLM agents
//! [`WorkerAgent`] / [`CriticAgent`]. These are what [`crate::run_task`]
//! wires up internally so the full LLM agent loop still works after
//! the executor extraction — but they're also useful for any caller
//! that wants the executor's DAG primitives PLUS an LLM-backed worker
//! / critic, without going through `run_task`'s replan loop.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use tars_pipeline::LlmService;

use crate::agent::Agent;
use crate::critic::{CriticAgent, CriticError, PartialResultRef};
use crate::executor::{Critic, CriticContext, Worker, WorkerContext, WorkerOutput};
use crate::message::{AgentMessage, VerdictKind};
use crate::orchestrator::{Plan, PlanStep};
use crate::runtime::execute_agent_step;
use crate::worker::{WorkerAgent, WorkerError};

/// [`Worker`] backed by a [`WorkerAgent`] + LLM. Builds the LLM
/// request via `WorkerAgent::build_worker_request` (threading
/// `prior_results` and `refinements`), drives it through
/// [`execute_agent_step`] for trajectory logging, then parses the
/// JSON response into the [`AgentMessage::PartialResult`] envelope.
pub struct LlmWorker {
    agent: Arc<WorkerAgent>,
    llm: Arc<dyn LlmService>,
}

impl LlmWorker {
    pub fn new(agent: Arc<WorkerAgent>, llm: Arc<dyn LlmService>) -> Arc<Self> {
        Arc::new(Self { agent, llm })
    }
}

#[async_trait]
impl Worker for LlmWorker {
    async fn run(
        &self,
        plan: &Plan,
        step: &PlanStep,
        prior_results: &HashMap<String, AgentMessage>,
        ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        let req = self
            .agent
            .build_worker_request(plan, step, &ctx.refinements, prior_results);
        let agent: Arc<dyn Agent> = self.agent.clone();
        let result = execute_agent_step(
            ctx.runtime.as_ref(),
            &ctx.trajectory_id,
            self.llm.clone(),
            agent,
            req,
            ctx.cancel.clone(),
        )
        .await
        .map_err(|e| match e {
            crate::runtime::AgentExecutionError::Agent(a) => WorkerError::Agent(a),
            crate::runtime::AgentExecutionError::Runtime(r) => {
                // Runtime errors during LLM step execution surface as
                // an internal-shape error — they're not LLM-call
                // failures per se, so don't pretend they are. The
                // executor surfaces this as RunPlanError::Worker which
                // run_task maps to RunTaskError::Worker.
                WorkerError::InvalidResult(format!("runtime: {r}"))
            }
        })?;
        let json_text = match result.output {
            crate::agent::AgentOutput::Text { text } => text,
            other => {
                return Err(WorkerError::UnexpectedOutput(format!(
                    "expected JSON text from worker; got {other:?}"
                )));
            }
        };
        let message = WorkerAgent::parse_worker_response(
            &json_text,
            self.agent.id(),
            Some(step.id.as_str()),
        )?;
        Ok(WorkerOutput {
            message,
            usage: result.usage,
            created: result.created,
        })
    }
}

/// [`Critic`] backed by a [`CriticAgent`] + LLM. Builds the critique
/// request via `CriticAgent::build_critique_request`, drives it
/// through [`execute_agent_step`], parses the verdict JSON.
pub struct LlmCritic {
    agent: Arc<CriticAgent>,
    llm: Arc<dyn LlmService>,
}

impl LlmCritic {
    pub fn new(agent: Arc<CriticAgent>, llm: Arc<dyn LlmService>) -> Arc<Self> {
        Arc::new(Self { agent, llm })
    }
}

#[async_trait]
impl Critic for LlmCritic {
    async fn judge(
        &self,
        plan: &Plan,
        step: &PlanStep,
        worker_output: &AgentMessage,
        ctx: CriticContext,
    ) -> Result<VerdictKind, CriticError> {
        let result_ref = PartialResultRef::from_message(worker_output).ok_or_else(|| {
            CriticError::UnexpectedOutput("worker did not produce a PartialResult message".into())
        })?;
        let req = self
            .agent
            .build_critique_request(plan, &result_ref, &plan.goal);
        let agent: Arc<dyn Agent> = self.agent.clone();
        let result = execute_agent_step(
            ctx.runtime.as_ref(),
            &ctx.trajectory_id,
            self.llm.clone(),
            agent,
            req,
            ctx.cancel.clone(),
        )
        .await
        .map_err(|e| match e {
            crate::runtime::AgentExecutionError::Agent(a) => CriticError::Agent(a),
            crate::runtime::AgentExecutionError::Runtime(r) => {
                CriticError::InvalidVerdict(format!("runtime: {r}"))
            }
        })?;
        let json_text = match result.output {
            crate::agent::AgentOutput::Text { text } => text,
            other => {
                return Err(CriticError::UnexpectedOutput(format!(
                    "expected JSON verdict; got {other:?}",
                )));
            }
        };
        let verdict_msg = CriticAgent::parse_verdict_response(
            &json_text,
            self.agent.id(),
            Some(step.id.as_str()),
        )?;
        match verdict_msg {
            AgentMessage::Verdict { verdict, .. } => Ok(verdict),
            other => Err(CriticError::UnexpectedOutput(format!(
                "expected Verdict envelope; got {other:?}",
            ))),
        }
    }
}
