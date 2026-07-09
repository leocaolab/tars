//! A pure-Rust (no-LLM) `run_plan` pipeline whose steps are **blackboard-based
//! Workers** — the shape `arc auto` migrates to: a `scan → fix → merge` chain
//! where every step reads/writes a shared blackboard and commits its own event
//! at source. No `Node`, no `NodeRunner`, no sealed write — a `Worker` IS the
//! step; it declares `reads`/`emits` and commits EXPLICITLY.
//!
//! Run with: `cargo run --example non_llm_plan -p tars-runtime`
//!
//! What this proves at the API surface:
//!
//! 1. `run_plan` takes a caller-built `Plan` — no LLM planning round; workers
//!    are plain async fns, `critic = None`.
//! 2. A `Worker` gained two DECLARATIONS — `reads()` + `emits()` (defaults, so
//!    existing workers are untouched) — readable BEFORE the run.
//! 3. `WorkerContext.shared` carries the run-scoped blackboard (type-erased;
//!    injected once via `RunPlanConfig.shared`; a worker downcasts it).
//! 4. Fan-out + dependency scheduling: two `fix` siblings run in parallel after
//!    `scan`, then `merge` consumes their result — and **state flows through the
//!    blackboard**, not through `prior_results` (deps carry ORDERING only).
//! 5. After the run, each finding's timeline is COMPLETE
//!    (found → fixed → merged) — the property the migration must guarantee.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use tokio_util::sync::CancellationToken;

use tars_runtime::{
    run_plan, AgentMessage, LocalRuntime, Plan, PlanStep, RunPlanConfig, Runtime, StepCondition,
    StepOutcome, Worker, WorkerContext, WorkerError, WorkerOutput, WorkerRegistry,
};
use tars_storage::{
    BbError, Blackboard, BlackboardDomain, BlackboardStore, Scope, SqliteBlackboard,
    SqliteAgentEventLog, Transition,
};
use tars_types::{AgentId, Usage};

// ── The consumer's domain: a code-review board (toy) ─────────────────────────

#[derive(Clone, Debug)]
struct Finding {
    id: String,
    title: String,
    status: String,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Ev {
    Found,
    Fixed,
    Merged,
}

struct ReviewBoard;

impl BlackboardDomain for ReviewBoard {
    type Entity = Finding;
    type Event = Ev;
    fn key(e: &Finding) -> String {
        e.id.clone()
    }
    fn initial_status(_e: &Finding) -> String {
        "new".into()
    }
    fn event_str(ev: Ev) -> String {
        match ev {
            Ev::Found => "found",
            Ev::Fixed => "fixed",
            Ev::Merged => "merged",
        }
        .into()
    }
    fn event_from_str(s: &str) -> Ev {
        match s {
            "fixed" => Ev::Fixed,
            "merged" => Ev::Merged,
            _ => Ev::Found,
        }
    }
    fn project_status(timeline: &[Ev]) -> Option<String> {
        // fixed → merged never downgrades; a bare `found` doesn't project.
        let rank = |e: Ev| match e {
            Ev::Fixed => 1,
            Ev::Merged => 2,
            Ev::Found => 0,
        };
        let mut st = None;
        let mut chain = 0;
        for &e in timeline {
            if rank(e) > chain {
                chain = rank(e);
                st = Some(Self::event_str(e));
            }
        }
        st
    }
    fn with_status(e: &Finding, status: &str) -> Finding {
        Finding { status: status.into(), ..e.clone() }
    }
}

impl BlackboardStore for ReviewBoard {
    fn init(conn: &Connection) -> Result<(), BbError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS rf (id TEXT PRIMARY KEY, title TEXT, status TEXT,
                 first_seen_run TEXT, last_seen_run TEXT);
             CREATE TABLE IF NOT EXISTS re (id TEXT, run TEXT, kind TEXT, at INTEGER,
                 PRIMARY KEY (id, run, kind));",
        )?;
        Ok(())
    }
    fn upsert(conn: &Connection, e: &Finding) -> Result<(), BbError> {
        conn.execute(
            "INSERT INTO rf (id,title,status,first_seen_run,last_seen_run) VALUES (?1,?2,?3,'r','r')
             ON CONFLICT(id) DO UPDATE SET title=excluded.title",
            params![e.id, e.title, Self::initial_status(e)],
        )?;
        Ok(())
    }
    fn append_event(
        conn: &Connection,
        e: &Finding,
        run: &str,
        ev: Ev,
        at: i64,
        _v: Option<&str>,
        _r: Option<&str>,
        _role: Option<&str>,
    ) -> Result<bool, BbError> {
        let n = conn.execute(
            "INSERT INTO re (id,run,kind,at) VALUES (?1,?2,?3,?4) ON CONFLICT DO NOTHING",
            params![e.id, run, Self::event_str(ev), at],
        )?;
        Ok(n > 0)
    }
    fn read_timeline(conn: &Connection, key: &str) -> Result<Vec<Ev>, BbError> {
        let mut s = conn.prepare("SELECT kind FROM re WHERE id=?1 ORDER BY at,rowid")?;
        let rows = s.query_map([key], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            let kind = r?;
            out.push(Self::event_from_str(&kind));
        }
        Ok(out)
    }
    fn sync_status(conn: &Connection, key: &str) -> Result<(), BbError> {
        let tl = Self::read_timeline(conn, key)?;
        if let Some(st) = Self::project_status(&tl) {
            conn.execute("UPDATE rf SET status=?2 WHERE id=?1", params![key, st])?;
        }
        Ok(())
    }
    fn view(conn: &Connection, scope: &Scope) -> Result<Vec<Finding>, BbError> {
        let mk =
            |r: &rusqlite::Row| Ok(Finding { id: r.get(0)?, title: r.get(1)?, status: r.get(2)? });
        let mut out = Vec::new();
        match scope {
            Scope::All => {
                let mut s = conn.prepare("SELECT id,title,status FROM rf ORDER BY id")?;
                for r in s.query_map([], mk)? {
                    out.push(r?);
                }
            }
            Scope::WithStatus(ss) => {
                if let Some(st) = ss.first() {
                    let mut s = conn
                        .prepare("SELECT id,title,status FROM rf WHERE status=?1 ORDER BY id")?;
                    for r in s.query_map([st], mk)? {
                        out.push(r?);
                    }
                }
            }
            Scope::FirstSeenIn(run) => {
                let mut s = conn
                    .prepare("SELECT id,title,status FROM rf WHERE first_seen_run=?1 ORDER BY id")?;
                for r in s.query_map([run], mk)? {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }
}

type Bb = SqliteBlackboard<ReviewBoard>;

/// A blackboard-aware worker pulls its handle out of the type-erased context.
fn bb_of(ctx: &WorkerContext) -> Arc<Bb> {
    ctx.shared
        .clone()
        .expect("blackboard injected via RunPlanConfig.shared")
        .downcast::<Bb>()
        .expect("shared is the ReviewBoard blackboard")
}

fn done(step: &PlanStep, summary: &str) -> WorkerOutput {
    WorkerOutput {
        message: AgentMessage::PartialResult {
            from_agent: AgentId::new(format!("w-{}", step.worker_role)),
            step_id: Some(step.id.clone()),
            summary: summary.into(),
            confidence: 1.0,
        },
        usage: Usage::default(),
        created: 0,
    }
}

// ── The blackboard-based workers ─────────────────────────────────────────────

/// Detects findings and commits a `found` for each (at source).
struct ScanWorker;
#[async_trait]
impl Worker for ScanWorker {
    fn reads(&self) -> Scope {
        Scope::All
    }
    fn emits(&self) -> Vec<String> {
        vec!["found".into()]
    }
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        let bb = bb_of(&ctx);
        for (id, title) in [("F-1", "swallowed exception"), ("F-2", "unwrap in library")] {
            let f = Finding { id: id.into(), title: title.into(), status: "new".into() };
            bb.commit(&f, Transition::new(Ev::Found, 100, Some("aaaa".into()))).unwrap();
            println!("  [{}] commit found  {id}  ({title})", step.id);
        }
        Ok(done(step, "2 findings"))
    }
}

/// Fixes the ONE finding named in `step.instruction` (fan-out: each sibling
/// handles its own). Reads it from the blackboard, commits `fixed`.
struct FixWorker;
#[async_trait]
impl Worker for FixWorker {
    fn reads(&self) -> Scope {
        Scope::WithStatus(vec!["new".into()])
    }
    fn emits(&self) -> Vec<String> {
        vec!["fixed".into()]
    }
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        let bb = bb_of(&ctx);
        let target = step.instruction.clone(); // which finding this sibling owns
        // STATE comes from the blackboard, not prior_results.
        let open = bb.view(&self.reads()).unwrap();
        if let Some(f) = open.into_iter().find(|f| f.id == target) {
            bb.commit(&f, Transition::new(Ev::Fixed, 200, Some("bbbb".into()))).unwrap();
            println!("  [{}] view→commit fixed  {}", step.id, f.id);
            Ok(done(step, &format!("fixed {}", f.id)))
        } else {
            Ok(done(step, "nothing to fix"))
        }
    }
}

/// Lands every fixed finding: commits `merged` for each. Reads its work from the
/// blackboard (NOT from its deps' messages — those only ordered it after fix).
struct MergeWorker;
#[async_trait]
impl Worker for MergeWorker {
    fn reads(&self) -> Scope {
        Scope::WithStatus(vec!["fixed".into()])
    }
    fn emits(&self) -> Vec<String> {
        vec!["merged".into()]
    }
    async fn run(
        &self,
        _plan: &Plan,
        step: &PlanStep,
        _prior: &HashMap<String, AgentMessage>,
        ctx: WorkerContext,
    ) -> Result<WorkerOutput, WorkerError> {
        let bb = bb_of(&ctx);
        let fixed = bb.view(&self.reads()).unwrap();
        for f in &fixed {
            bb.commit(f, Transition::new(Ev::Merged, 300, Some("cccc".into()))).unwrap();
            println!("  [{}] view→commit merged  {}", step.id, f.id);
        }
        Ok(done(step, &format!("merged {}", fixed.len())))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("non-LLM run_plan pipeline over a blackboard — workers commit explicitly\n");

    // The run-scoped blackboard the whole pipeline shares.
    let bb: Arc<Bb> = Arc::new(SqliteBlackboard::<ReviewBoard>::in_memory("run-1")?);

    let runtime: Arc<dyn Runtime> = LocalRuntime::new(SqliteAgentEventLog::in_memory()?);
    let mut registry = WorkerRegistry::new();
    registry.register("scan", Arc::new(ScanWorker));
    registry.register("fix", Arc::new(FixWorker));
    registry.register("merge", Arc::new(MergeWorker));

    // 3-level DAG: scan → {fix F-1, fix F-2 in parallel} → merge.
    let plan = Plan {
        plan_id: "non-llm-bb-demo".into(),
        goal: "scan → fix (fan-out) → merge, over a blackboard".into(),
        steps: vec![
            PlanStep {
                id: "scan".into(),
                worker_role: "scan".into(),
                instruction: "scan the repo".into(),
                depends_on: vec![],
                condition: StepCondition::Always,
            },
            PlanStep {
                id: "fix-a".into(),
                worker_role: "fix".into(),
                instruction: "F-1".into(),
                depends_on: vec!["scan".into()],
                condition: StepCondition::Always,
            },
            PlanStep {
                id: "fix-b".into(),
                worker_role: "fix".into(),
                instruction: "F-2".into(),
                depends_on: vec!["scan".into()],
                condition: StepCondition::Always,
            },
            PlanStep {
                id: "merge".into(),
                worker_role: "merge".into(),
                instruction: "land the fixes".into(),
                depends_on: vec!["fix-a".into(), "fix-b".into()],
                condition: StepCondition::Always,
            },
        ],
    };

    // declared dataflow — readable BEFORE running, because reads/emits are data:
    println!("declared pipeline:");
    let decl: [(&str, &dyn Worker); 3] =
        [("scan", &ScanWorker), ("fix", &FixWorker), ("merge", &MergeWorker)];
    for (role, w) in decl {
        println!("  {role:<5} reads={:?}  emits={:?}", w.reads(), w.emits());
    }
    println!();

    let traj = runtime.create_trajectory(None, "non-llm bb demo").await?;
    let config = RunPlanConfig {
        shared: Some(bb.clone() as Arc<dyn Any + Send + Sync>), // inject the blackboard
        ..Default::default()
    };
    let outcome =
        run_plan(runtime.clone(), traj, plan, registry, None, config, CancellationToken::new())
            .await?;

    println!("\n══ step outcomes ════════════════════════════════════════");
    for s in &outcome.steps {
        if let StepOutcome::Completed {
            step_id,
            result: AgentMessage::PartialResult { summary, .. },
            ..
        } = s
        {
            println!("  {step_id:<7} — {summary}");
        }
    }

    // The blackboard is the source of truth — read the final state back.
    println!("\n══ blackboard after the run ═════════════════════════════");
    for f in bb.view(&Scope::All)? {
        let tl: Vec<String> =
            bb.timeline(&f.id)?.iter().map(|e| ReviewBoard::event_str(*e)).collect();
        println!("  {}: status={:<6} timeline={:?}", f.id, f.status, tl);
    }
    println!("\n⇒ every step committed its own event; timelines are COMPLETE (found→fixed→merged).");
    Ok(())
}
