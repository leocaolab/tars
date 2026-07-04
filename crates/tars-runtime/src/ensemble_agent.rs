//! [`EnsembleAgent`] — agent-level hedging over `tars_model::Agent`.
//!
//! Doc 19/20: routing/ensemble lifts to the **Agent** (task granularity),
//! not just the LlmService (completion granularity). An `EnsembleAgent`
//! runs ONE [`Task`] on N candidate agents concurrently, returns the FIRST
//! that succeeds, and cancels the rest. That's how a tool-using fixer gets
//! tail-latency hedging — `claude_cli agent` vs `gemini agent` vs a user
//! agent, first good result wins — which the pipeline's completion-level
//! ensemble can't give a multi-turn agent.
//!
//! Because it composes over the `Agent` trait, it's blind to whether a
//! candidate is native or user-implemented: tars adapts both.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::{BoxFuture, select_all};

use tars_model::{
    Agent, AgentContext, TaskError, AgentId, AgentOutput, AgentRole, SkillSet, Task,
};

/// Runs a Task on several candidate agents at once; first success wins.
pub struct EnsembleAgent {
    id: AgentId,
    role: AgentRole,
    skills: SkillSet,
    candidates: Vec<Arc<dyn Agent>>,
}

impl EnsembleAgent {
    /// Build an ensemble. `role` is the ensemble's own role (usually the
    /// shared role of its candidates); `skills` advertises the UNION of
    /// what the candidates can do.
    pub fn new(id: impl Into<String>, role: AgentRole, candidates: Vec<Arc<dyn Agent>>) -> Self {
        // Union the candidates' skills — the ensemble can do whatever any
        // member can.
        let mut skills = SkillSet::new();
        for c in &candidates {
            for s in c.skills().iter() {
                skills = skills.with(s.clone());
            }
        }
        Self {
            id: AgentId::new(id),
            role,
            skills,
            candidates,
        }
    }

    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }
}

#[async_trait]
impl Agent for EnsembleAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn role(&self) -> &AgentRole {
        &self.role
    }

    fn skills(&self) -> &SkillSet {
        &self.skills
    }

    async fn run(&self, task: Task, ctx: AgentContext) -> Result<AgentOutput, TaskError> {
        if self.candidates.is_empty() {
            return Err(TaskError::Execution("ensemble has no candidates".into()));
        }

        // Each candidate gets a CHILD cancel token: a parent cancel still
        // stops everyone, but the winner cancelling the losers doesn't
        // touch the parent.
        let mut futures: Vec<BoxFuture<'static, Result<AgentOutput, TaskError>>> =
            Vec::with_capacity(self.candidates.len());
        let tokens: Vec<_> = self
            .candidates
            .iter()
            .map(|_| ctx.cancel.child_token())
            .collect();

        for (cand, token) in self.candidates.iter().zip(tokens.iter()) {
            let cand = Arc::clone(cand);
            let task = task.clone();
            let mut cctx = ctx.clone();
            cctx.cancel = token.clone();
            futures.push(Box::pin(async move { cand.run(task, cctx).await }));
        }

        let cancel_all = || tokens.iter().for_each(|t| t.cancel());

        let mut last_err: Option<TaskError> = None;
        let mut remaining = futures;
        while !remaining.is_empty() {
            let (result, _idx, rest) = select_all(remaining).await;
            match result {
                Ok(out) => {
                    // Winner — stop the losers.
                    cancel_all();
                    return Ok(out);
                }
                Err(TaskError::Cancelled) => {
                    // A sibling/parent cancel raced in; keep waiting on the
                    // others rather than failing the whole ensemble.
                    last_err = Some(TaskError::Cancelled);
                    remaining = rest;
                }
                Err(e) => {
                    last_err = Some(e);
                    remaining = rest;
                }
            }
        }

        // Everyone failed.
        Err(last_err.unwrap_or_else(|| TaskError::Execution("all candidates failed".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_model::{Skill, TaskId};

    /// A canned agent: either succeeds with a summary or fails.
    struct CannedAgent {
        id: AgentId,
        role: AgentRole,
        skills: SkillSet,
        result: Result<String, String>,
    }

    impl CannedAgent {
        fn ok(id: &str, skill: &str, summary: &str) -> Arc<dyn Agent> {
            Arc::new(Self {
                id: AgentId::new(id),
                role: AgentRole::worker("test"),
                skills: SkillSet::new().with(Skill::new(skill, "x")),
                result: Ok(summary.to_string()),
            })
        }
        fn fail(id: &str, skill: &str) -> Arc<dyn Agent> {
            Arc::new(Self {
                id: AgentId::new(id),
                role: AgentRole::worker("test"),
                skills: SkillSet::new().with(Skill::new(skill, "x")),
                result: Err("boom".to_string()),
            })
        }
    }

    #[async_trait]
    impl Agent for CannedAgent {
        fn id(&self) -> &AgentId {
            &self.id
        }
        fn role(&self) -> &AgentRole {
            &self.role
        }
        fn skills(&self) -> &SkillSet {
            &self.skills
        }
        async fn run(&self, _t: Task, _c: AgentContext) -> Result<AgentOutput, TaskError> {
            match &self.result {
                Ok(s) => Ok(AgentOutput::new(s.clone())),
                Err(e) => Err(TaskError::Execution(e.clone())),
            }
        }
    }

    fn task() -> Task {
        Task::new(TaskId::new("t"), "do it")
    }

    #[tokio::test]
    async fn first_success_wins_over_a_failure() {
        let ens = EnsembleAgent::new(
            "ens",
            AgentRole::worker("test"),
            vec![
                CannedAgent::fail("a", "fs.edit"),
                CannedAgent::ok("b", "fs.write", "b did it"),
            ],
        );
        // Union of skills advertised.
        assert!(ens.skills().contains("fs.edit") && ens.skills().contains("fs.write"));
        let out = ens.run(task(), AgentContext::new()).await.unwrap();
        assert_eq!(out.summary, "b did it");
    }

    #[tokio::test]
    async fn all_failing_returns_an_error() {
        let ens = EnsembleAgent::new(
            "ens",
            AgentRole::worker("test"),
            vec![CannedAgent::fail("a", "x"), CannedAgent::fail("b", "y")],
        );
        assert!(ens.run(task(), AgentContext::new()).await.is_err());
    }

    #[tokio::test]
    async fn empty_ensemble_errors() {
        let ens = EnsembleAgent::new("ens", AgentRole::worker("test"), vec![]);
        assert!(ens.run(task(), AgentContext::new()).await.is_err());
    }
}
