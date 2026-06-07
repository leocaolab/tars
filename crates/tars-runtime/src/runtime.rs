//! [`Runtime`] trait + [`LocalRuntime`] impl.
//!
//! M3 first cut: thin facade over [`tars_storage::EventStore`] that
//! handles trajectory creation + typed-event append/read. The Agent
//! execution loop (Doc 04 §4) lives in a follow-on commit alongside
//! the actual `Agent` trait, prompt builder, tool registry, etc.

use std::sync::Arc;

use async_trait::async_trait;

use tars_pipeline::LlmService;
use tars_storage::EventStore;
use tars_types::{ChatRequest, TrajectoryId};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::{Agent, AgentContext, AgentError, AgentStepResult};
use crate::error::RuntimeError;
use crate::event::{AgentEvent, StepIdempotencyKey};

/// Top-level runtime facade. Implementors decide how to back the
/// event store (SQLite for personal mode, Postgres for team mode);
/// the trait surface stays the same so consumers don't care.
#[async_trait]
pub trait Runtime: Send + Sync + 'static {
    /// Mint a fresh trajectory and write the inaugural
    /// `TrajectoryStarted` event. Returns the new id so callers can
    /// thread it through subsequent `append` calls.
    ///
    /// `parent` is `None` for a root trajectory; `Some(parent_id)`
    /// for branches (replan / fork / recovery — Doc 04 §3.1
    /// `BranchReason`).
    async fn create_trajectory(
        &self,
        parent: Option<TrajectoryId>,
        reason: &str,
    ) -> Result<TrajectoryId, RuntimeError>;

    /// Append a typed event. Returns the assigned `sequence_no`.
    async fn append(&self, traj: &TrajectoryId, event: AgentEvent) -> Result<u64, RuntimeError>;

    /// Replay every event for `traj` in order. Returns an empty `Vec`
    /// (NOT `TrajectoryNotFound`) when the trajectory has no events
    /// — "haven't appended yet" is a normal state.
    async fn replay(&self, traj: &TrajectoryId) -> Result<Vec<AgentEvent>, RuntimeError>;

    /// Replay events with `sequence_no > since`. Used by recovery /
    /// incremental projections.
    async fn replay_since(
        &self,
        traj: &TrajectoryId,
        since: u64,
    ) -> Result<Vec<AgentEvent>, RuntimeError>;

    /// All known trajectory ids (for admin / recovery scans).
    async fn list_trajectories(&self) -> Result<Vec<TrajectoryId>, RuntimeError>;

    /// True iff the trajectory's last event is terminal
    /// ([`AgentEvent::is_terminal`]). Cheap convenience for code
    /// that wants to "skip resumable trajectories" without pulling
    /// the whole event log.
    async fn is_terminated(&self, traj: &TrajectoryId) -> Result<bool, RuntimeError> {
        let events = self.replay(traj).await?;
        Ok(events.last().is_some_and(AgentEvent::is_terminal))
    }

    /// Count `StepStarted` events for `traj` (the trajectory's logical
    /// step count, used to assign the next `step_seq`).
    ///
    /// The default impl replays the whole log and counts — fine for
    /// short trajectories but O(events) in memory, so a long-running
    /// trajectory risks OOM. A storage-aware impl SHOULD override this
    /// with a bounded query (e.g. SQLite
    /// `SELECT COUNT(*) ... WHERE payload->>'type' = 'step_started'`),
    /// which needs no in-memory materialization. Extracted as its own
    /// trait method precisely so that override can land without
    /// touching `execute_agent_step`.
    async fn count_started_steps(&self, traj: &TrajectoryId) -> Result<u32, RuntimeError> {
        let events = self.replay(traj).await?;
        let n = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::StepStarted { .. }))
            .count();
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Allocate the next `step_seq` for `traj`. **Must be linearisable
    /// across concurrent callers** when the runtime is used to drive
    /// parallel agent execution (e.g. a DAG executor running
    /// independent plan steps in flight at the same time).
    ///
    /// The default impl is `count_started_steps + 1`, which is
    /// race-prone under concurrency: two callers can both read
    /// `count = N` and both append events claiming `step_seq = N+1`.
    /// That collision is invisible to the event store (no unique
    /// constraint on step_seq) but corrupts the invariant that
    /// step_seq monotonically identifies a logical step.
    ///
    /// Storage-aware / runtime-owned impls SHOULD override this with
    /// a serialised counter (mutex-protected, cached after first DB
    /// read). `LocalRuntime` does so. The default lives on for test
    /// mocks where parallelism isn't exercised.
    async fn next_step_seq(&self, traj: &TrajectoryId) -> Result<u32, RuntimeError> {
        Ok(self.count_started_steps(traj).await?.saturating_add(1))
    }
}

/// Production runtime backed by an [`EventStore`].
///
/// Stateless beyond the event-store handle; cheap to clone via
/// `Arc::clone`. Construction is a one-liner: pass any `EventStore`
/// (today the SQLite impl; tomorrow Postgres).
#[derive(Clone)]
pub struct LocalRuntime {
    event_store: Arc<dyn EventStore>,
    /// Per-trajectory cached `next step_seq` for the parallel-safe
    /// allocator (`next_step_seq` override). Lazily initialised on
    /// first allocation by replaying `count_started_steps`; all
    /// subsequent allocations under the same trajectory hit the cache
    /// and increment under the mutex. Trajectories are short-lived
    /// per run, so the HashMap never grows unbounded in practice; if
    /// it ever did, an LRU layer would slot in here without API churn.
    step_seq_counters: Arc<tokio::sync::Mutex<std::collections::HashMap<TrajectoryId, u32>>>,
}

impl LocalRuntime {
    pub fn new(event_store: Arc<dyn EventStore>) -> Arc<Self> {
        Arc::new(Self {
            event_store,
            step_seq_counters: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Mint a fresh trajectory id. Default: `uuid v4` formatted as
    /// hex. Exposed as a separate fn so tests can monkey-patch via
    /// a custom `Runtime` impl when they want predictable ids.
    fn fresh_trajectory_id() -> TrajectoryId {
        TrajectoryId::new(Uuid::new_v4().simple().to_string())
    }
}

#[async_trait]
impl Runtime for LocalRuntime {
    async fn create_trajectory(
        &self,
        parent: Option<TrajectoryId>,
        reason: &str,
    ) -> Result<TrajectoryId, RuntimeError> {
        let traj = Self::fresh_trajectory_id();
        let event = AgentEvent::TrajectoryStarted {
            traj: traj.clone(),
            parent,
            reason: reason.to_string(),
        };
        let payload = serde_json::to_value(&event)?;
        // The returned sequence_no is deliberately ignored:
        // `TrajectoryStarted` is invariantly the first event of a fresh
        // trajectory (event_seq == 1, see `execute_agent_step` step 1),
        // and the caller identifies the trajectory by the returned
        // `TrajectoryId`, not by this event's seq. Nothing downstream
        // needs the number here.
        let _seq = self.event_store.append(&traj, &[payload]).await?;
        tracing::debug!(
            trajectory_id = %traj,
            reason,
            "runtime: trajectory created",
        );
        Ok(traj)
    }

    async fn append(&self, traj: &TrajectoryId, event: AgentEvent) -> Result<u64, RuntimeError> {
        // Defensive — surface the obvious bug ("appended an event
        // claiming to be in trajectory A while passing trajectory B")
        // at the runtime layer rather than letting the row land
        // misfiled in the store.
        if event.trajectory_id() != traj {
            return Err(RuntimeError::TrajectoryMismatch {
                event: event.trajectory_id().to_string(),
                target: traj.to_string(),
            });
        }
        let payload = serde_json::to_value(&event)?;
        let seq = self.event_store.append(traj, &[payload]).await?;
        Ok(seq)
    }

    async fn replay(&self, traj: &TrajectoryId) -> Result<Vec<AgentEvent>, RuntimeError> {
        self.replay_since(traj, 0).await
    }

    async fn replay_since(
        &self,
        traj: &TrajectoryId,
        since: u64,
    ) -> Result<Vec<AgentEvent>, RuntimeError> {
        let records = self.event_store.read_since(traj, since).await?;
        records
            .into_iter()
            .map(|r| serde_json::from_value::<AgentEvent>(r.payload).map_err(RuntimeError::Serde))
            .collect()
    }

    async fn list_trajectories(&self) -> Result<Vec<TrajectoryId>, RuntimeError> {
        Ok(self.event_store.list_trajectories().await?)
    }

    /// Parallel-safe `step_seq` allocator. Holds a per-trajectory
    /// in-memory counter under a mutex, populated lazily from the
    /// event store on first allocation. Subsequent allocations are
    /// pure cache hits, so concurrent `execute_agent_step` calls from
    /// a DAG executor each get a distinct, monotonically increasing
    /// `step_seq` with no race window.
    ///
    /// The default trait impl (`count_started_steps + 1`) is
    /// race-prone — two parallel callers both read N and both write
    /// N+1. This override is what makes parallel run_task safe.
    async fn next_step_seq(&self, traj: &TrajectoryId) -> Result<u32, RuntimeError> {
        // Fast path: cache hit. Increment in place under the mutex,
        // observers serialise on the lock so two parallel callers see
        // distinct successive values.
        {
            let mut guard = self.step_seq_counters.lock().await;
            if let Some(c) = guard.get_mut(traj) {
                let next = c.saturating_add(1);
                *c = next;
                return Ok(next);
            }
        }
        // Slow path: lazy seed from the event store. Run OUTSIDE the
        // mutex so storage I/O doesn't serialise unrelated trajectories.
        // Multiple threads racing here may both query — that's fine, the
        // `entry().or_insert` below resolves on the LAST writer winning
        // *only when first seeding*; once seeded, the in-place
        // increment guarantees monotonic distinct values for all
        // subsequent callers.
        let from_store = self.count_started_steps(traj).await?;
        let mut guard = self.step_seq_counters.lock().await;
        let entry = guard.entry(traj.clone()).or_insert(from_store);
        let next = entry.saturating_add(1);
        *entry = next;
        Ok(next)
    }
}

/// Drive `agent.execute()` through one trajectory step, with full
/// event-log wrapping. The pattern is the same one `tars-cli`'s
/// `run.rs` builds inline today; this function is the
/// reusable-by-future-orchestrators version.
///
/// Lifecycle on the trajectory:
/// 1. Compute `step_seq = high_water + 1`.
/// 2. Append `AgentEvent::StepStarted` with idempotency key.
/// 3. Build `AgentContext`; call `agent.execute(ctx, input)`.
/// 4. On Ok: append `LlmCallCaptured` (one per step — multi-call
///    agents come later) + `StepCompleted`, return the result.
/// 5. On Err: append `StepFailed` with classification, return error.
///
/// **Does not** write `TrajectoryStarted` / `TrajectoryCompleted` /
/// `TrajectoryAbandoned` — those are the orchestrator's concern
/// (one trajectory may run many steps before completing). Caller is
/// responsible for closing the trajectory when its work is done.
///
/// Trajectory writes that fail get propagated as
/// [`RuntimeError::Storage`] — unlike the CLI's "best-effort,
/// degrade silently" pattern, here we're an internal building block
/// where storage failures matter (the next step's `step_seq` would
/// be wrong if a `StepStarted` write silently dropped).
///
/// ## Crash / recovery contract (NOT exactly-once)
///
/// This function is **at-most-once-effect, at-least-once-attempt**, not
/// exactly-once. A crash between step 2 (`StepStarted`) and the
/// terminal event (step 4 `StepCompleted` / step 5 `StepFailed`) leaves
/// a `StepStarted` with no terminal — an *orphan*. Recovery is the
/// caller's job and works as follows:
///
/// - Each step carries a [`StepIdempotencyKey`] = hash of
///   `(traj, step_seq, input_summary)`. On retry, the recomputed key
///   for the same logical step is identical, so a recovery pass can
///   detect "I already started step N" by scanning for a `StepStarted`
///   whose key matches and which lacks a terminal event.
/// - This function does **not** itself dedupe on that key — appending
///   is unconditional. An orphaned `StepStarted` followed by a fresh
///   call to `execute_agent_step` will allocate `step_seq = orphan + 1`
///   (the orphan still counts), so the orphan is left in place as an
///   audit record and the retry runs as a new step. Callers that want
///   true exactly-once must check `count_started_steps` against an
///   expected value (or scan for a key match) before re-invoking.
///
/// In short: side effects inside `agent.execute` (the LLM call) may run
/// more than once across a crash+retry; the event log is the source of
/// truth for what actually completed.
pub async fn execute_agent_step(
    runtime: &dyn Runtime,
    traj: &TrajectoryId,
    llm: Arc<dyn LlmService>,
    agent: Arc<dyn Agent>,
    input: ChatRequest,
    cancel: CancellationToken,
) -> Result<AgentStepResult, AgentExecutionError> {
    // 1. step_seq = (count of existing StepStarted events) + 1.
    //    NOT event high-water + 1 — that conflates "trajectory's
    //    Nth event" with "trajectory's Nth step", off-by-one'ing the
    //    very first step (TrajectoryStarted occupies event_seq=1, so
    //    a fresh trajectory's first step would otherwise come out
    //    as step_seq=2). Doc 04 §3.2 invariant 3 makes step_seq the
    //    LOGICAL step identifier; event sequencing is orthogonal.
    // Parallel-safe allocation: under a DAG executor with concurrent
    // step calls, the bare `count + 1` shape would race (two callers
    // both read N, both append step_seq=N+1). `next_step_seq` is the
    // serialised entry point — production runtimes (LocalRuntime)
    // override it with a mutex-protected counter; test stubs default
    // to the historical race-prone shape, which is fine when calls
    // are serial.
    let step_seq: u32 = runtime.next_step_seq(traj).await?;

    // 2. StepStarted
    let input_summary = format!(
        "agent={} model={} messages={}",
        agent.id(),
        input.model.label(),
        input.messages.len()
    );
    let idempotency_key = StepIdempotencyKey::compute(traj, step_seq, &input_summary);
    runtime
        .append(
            traj,
            AgentEvent::StepStarted {
                traj: traj.clone(),
                step_seq,
                agent: agent.id().to_string(),
                idempotency_key,
                input_summary,
            },
        )
        .await
        .map_err(AgentExecutionError::Runtime)?;

    // 3. agent.execute()
    // Snapshot the system-prompt hash BEFORE moving `input` into the
    // agent — Doc 04 §3.2's audit pin. (Was tagged "TODO L-1
    // enterprise follow-on" against a roadmap entry that's since
    // been retired; the hash itself ships here, the broader SOC2 /
    // ISO surface around it remains future work without a tracking id.)
    // Plain SHA256 of the bytes so an external auditor can verify with
    // `sha256sum read_file.txt`.
    //
    // No TOCTOU: `input` is MOVED into `execute` below, so the agent
    // gets its own owned copy. Anything it mutates is private and
    // cannot retroactively change the bytes hashed here. The contract
    // this hash records is "the system prompt as handed to the agent",
    // which is the audit fact we pin. (An agent that rewrites
    // `input.system` before its own LLM call would send a different
    // prompt than this hash; today's agents — SingleShot / Critic /
    // Worker — pass `input` through unchanged, so the two coincide.)
    let provider_for_log = guess_provider_id(&input);
    let system_prompt_hash = crate::event::hash_system_prompt(input.system.as_deref());
    let ctx = AgentContext {
        trajectory_id: traj.clone(),
        step_seq,
        llm,
        cancel,
        // run_plan agents act on the process cwd; a per-step worktree
        // would flow in here once PlanStep carries one.
        cwd: None,
    };
    let result = agent.clone().execute(ctx, input).await;

    // 4 / 5. log outcome
    match result {
        Ok(step_result) => {
            runtime
                .append(
                    traj,
                    AgentEvent::LlmCallCaptured {
                        traj: traj.clone(),
                        step_seq,
                        provider: provider_for_log.clone(),
                        prompt_summary: format!("agent={}", agent.id()),
                        response_summary: step_result.output.summary(200),
                        usage: step_result.usage,
                        system_prompt_hash,
                    },
                )
                .await
                .map_err(AgentExecutionError::Runtime)?;
            runtime
                .append(
                    traj,
                    AgentEvent::StepCompleted {
                        traj: traj.clone(),
                        step_seq,
                        output_summary: step_result.output.summary(200),
                        usage: step_result.usage,
                    },
                )
                .await
                .map_err(AgentExecutionError::Runtime)?;
            Ok(step_result)
        }
        Err(agent_err) => {
            let classification = agent_err.classification().to_string();
            runtime
                .append(
                    traj,
                    AgentEvent::StepFailed {
                        traj: traj.clone(),
                        step_seq,
                        error: format!("{agent_err}"),
                        classification,
                    },
                )
                .await
                .map_err(AgentExecutionError::Runtime)?;
            Err(AgentExecutionError::Agent(agent_err))
        }
    }
}

/// Best-effort: stamp the LlmCallCaptured event with which provider
/// was targeted. Today every consumer passes `ModelHint::Explicit`,
/// and the provider id ≈ "the model name" at this layer (the
/// pipeline's RoutingService picks the actual provider). When
/// Routing surfaces "which provider actually answered" through the
/// stream, this becomes the real value; until then it's a label.
fn guess_provider_id(req: &ChatRequest) -> tars_types::ProviderId {
    tars_types::ProviderId::new(req.model.label())
}

/// Errors that escape from [`execute_agent_step`]. Splits Agent
/// failures (the model said no, the prompt was malformed) from
/// Runtime failures (the event store is down).
#[derive(Debug, thiserror::Error)]
pub enum AgentExecutionError {
    #[error("agent: {0}")]
    Agent(#[from] AgentError),
    #[error("runtime: {0}")]
    Runtime(#[from] RuntimeError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_storage::SqliteEventStore;
    use tars_types::{ProviderId, Usage};

    use crate::event::StepIdempotencyKey;

    async fn fresh() -> Arc<LocalRuntime> {
        let store: Arc<dyn EventStore> = SqliteEventStore::in_memory().unwrap();
        LocalRuntime::new(store)
    }

    #[tokio::test]
    async fn create_trajectory_writes_started_event() {
        let rt = fresh().await;
        let traj = rt.create_trajectory(None, "root").await.unwrap();
        let events = rt.replay(&traj).await.unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::TrajectoryStarted { reason, parent, .. } => {
                assert_eq!(reason, "root");
                assert!(parent.is_none());
            }
            other => panic!("expected TrajectoryStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fresh_trajectory_ids_are_unique() {
        let rt = fresh().await;
        let a = rt.create_trajectory(None, "a").await.unwrap();
        let b = rt.create_trajectory(None, "b").await.unwrap();
        assert_ne!(a, b, "two creates should mint distinct ids");
    }

    #[tokio::test]
    async fn append_and_replay_round_trip_typed_events() {
        let rt = fresh().await;
        let traj = rt.create_trajectory(None, "t").await.unwrap();

        let key = StepIdempotencyKey::compute(&traj, 1, "input");
        rt.append(
            &traj,
            AgentEvent::StepStarted {
                traj: traj.clone(),
                step_seq: 1,
                agent: "orchestrator".into(),
                idempotency_key: key.clone(),
                input_summary: "input".into(),
            },
        )
        .await
        .unwrap();
        rt.append(
            &traj,
            AgentEvent::LlmCallCaptured {
                traj: traj.clone(),
                step_seq: 1,
                provider: ProviderId::new("openai_main"),
                prompt_summary: "system + user".into(),
                response_summary: "haiku of borrow checker".into(),
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 18,
                    ..Default::default()
                },
                system_prompt_hash: None,
            },
        )
        .await
        .unwrap();
        rt.append(
            &traj,
            AgentEvent::StepCompleted {
                traj: traj.clone(),
                step_seq: 1,
                output_summary: "ok".into(),
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 18,
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
        rt.append(
            &traj,
            AgentEvent::TrajectoryCompleted {
                traj: traj.clone(),
                summary: "done".into(),
            },
        )
        .await
        .unwrap();

        let events = rt.replay(&traj).await.unwrap();
        assert_eq!(events.len(), 5);
        assert!(matches!(events[0], AgentEvent::TrajectoryStarted { .. }));
        assert!(matches!(events[1], AgentEvent::StepStarted { .. }));
        assert!(matches!(events[2], AgentEvent::LlmCallCaptured { .. }));
        assert!(matches!(events[3], AgentEvent::StepCompleted { .. }));
        assert!(matches!(events[4], AgentEvent::TrajectoryCompleted { .. }));
    }

    #[tokio::test]
    async fn mismatched_trajectory_in_event_is_rejected() {
        let rt = fresh().await;
        let real = rt.create_trajectory(None, "real").await.unwrap();
        let other = TrajectoryId::new("bogus");
        let result = rt
            .append(
                &real,
                AgentEvent::TrajectoryCompleted {
                    traj: other,
                    summary: "wrong target".into(),
                },
            )
            .await;
        match result {
            Err(RuntimeError::TrajectoryMismatch { event, target }) => {
                assert_eq!(event, "bogus");
                assert_eq!(target, real.as_str());
            }
            other => panic!("expected mismatch error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn replay_unknown_trajectory_returns_empty_not_error() {
        let rt = fresh().await;
        let events = rt
            .replay(&TrajectoryId::new("never_created"))
            .await
            .unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn is_terminated_tracks_completed_and_abandoned() {
        let rt = fresh().await;
        let traj = rt.create_trajectory(None, "t").await.unwrap();
        assert!(
            !rt.is_terminated(&traj).await.unwrap(),
            "Started is not terminal"
        );
        rt.append(
            &traj,
            AgentEvent::TrajectoryCompleted {
                traj: traj.clone(),
                summary: "ok".into(),
            },
        )
        .await
        .unwrap();
        assert!(rt.is_terminated(&traj).await.unwrap());
    }

    #[tokio::test]
    async fn replay_since_skips_known_prefix() {
        let rt = fresh().await;
        let traj = rt.create_trajectory(None, "t").await.unwrap();
        for i in 0..3 {
            rt.append(
                &traj,
                AgentEvent::TrajectorySuspended {
                    traj: traj.clone(),
                    reason: format!("pause-{i}"),
                },
            )
            .await
            .unwrap();
        }
        // Total: 1 (Started) + 3 (Suspended) = 4 events. Replay since
        // seq=2 should give us seq=3, seq=4 → two events.
        let tail = rt.replay_since(&traj, 2).await.unwrap();
        assert_eq!(tail.len(), 2);
    }

    #[tokio::test]
    async fn list_trajectories_includes_every_created() {
        let rt = fresh().await;
        let a = rt.create_trajectory(None, "a").await.unwrap();
        let b = rt.create_trajectory(None, "b").await.unwrap();
        let c = rt.create_trajectory(None, "c").await.unwrap();
        let mut listed = rt.list_trajectories().await.unwrap();
        listed.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        let mut expected = vec![a, b, c];
        expected.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        assert_eq!(listed, expected);
    }
}
