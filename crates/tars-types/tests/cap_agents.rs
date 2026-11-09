//! Runs three agents end to end through gated caps over an in-memory world, and shows that
//! the gate, not the agent, decides whether an `External` invocation happens.
//!
//! The caps are `fs.read`, `fs.grep`, `fs.write` (Read, Read, WriteLocal) and `egress`
//! (External). `RenameAgent` loops read, edit, verify until nothing matches; `ExfilAgent`
//! egresses a file, and the same agent runs, parks or runs after approval depending only
//! on the gate it is granted. Nothing touches a disk, a clock or randomness.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tars_types::CapCtx;

use tars_types::{
    AgentAsk, AgentReply, ApprovalRequest, ApprovalSink, Approver, Approvers, Cap, CapError,
    CapName, CapSpec, Effect, EffectTierGate, GatedCap, Goal, InvalidArgs, Outcome, Reason, Refs,
    SimpleGoal, Step, StepContext, StrIntent, Wait,
};

/// The files the caps act on, path to content, shared by the caps and the test.
type World = Arc<Mutex<BTreeMap<String, String>>>;

/// What `egress` sent out of the box; a test reads it to see whether the gate let the
/// egress happen.
type Wire = Arc<Mutex<Vec<Value>>>;

/// Returns the string field `key` of `args`, or [`CapError::Invalid`] when it is missing or
/// not a string.
fn str_arg(args: &Value, key: &str) -> Result<String, CapError> {
    match args.get(key).and_then(Value::as_str) {
        Some(s) => Ok(s.to_string()),
        None => Err(CapError::Invalid(InvalidArgs::Rejected(format!(
            "expected string field `{key}` in args, got: {args}"
        )))),
    }
}

/// Locks the world, turning a poisoned lock into [`CapError::ExecuteFatal`].
fn lock_world(
    world: &World,
) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, String>>, CapError> {
    world
        .lock()
        .map_err(|e| CapError::ExecuteFatal(format!("world lock poisoned: {e}")))
}

struct FsReadCap {
    world: World,
}

#[async_trait]
impl Cap for FsReadCap {
    fn spec(&self) -> CapSpec {
        CapSpec {
            name: CapName::new("fs.read"),
            doc: "Read the full content of one file at `path` from the worktree.".into(),
            in_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            out_schema: json!({ "type": "string", "description": "the file content" }),
        }
    }

    fn effect(&self) -> Effect {
        Effect::Read
    }

    async fn invoke(&self, args: Value, _ctx: &CapCtx) -> Result<Value, CapError> {
        let path = str_arg(&args, "path")?;
        let world = lock_world(&self.world)?;
        match world.get(&path) {
            Some(content) => Ok(json!(content)),
            None => Err(CapError::ExecuteFatal(format!("no such file: {path}"))),
        }
    }
}

/// Lists the paths whose content contains `pattern`, sorted because the world is a
/// `BTreeMap`.
struct FsGrepCap {
    world: World,
}

#[async_trait]
impl Cap for FsGrepCap {
    fn spec(&self) -> CapSpec {
        CapSpec {
            name: CapName::new("fs.grep"),
            doc: "List the paths of files whose content contains the substring `pattern`.".into(),
            in_schema: json!({
                "type": "object",
                "properties": { "pattern": { "type": "string" } },
                "required": ["pattern"]
            }),
            out_schema: json!({ "type": "array", "items": { "type": "string" } }),
        }
    }

    fn effect(&self) -> Effect {
        Effect::Read
    }

    async fn invoke(&self, args: Value, _ctx: &CapCtx) -> Result<Value, CapError> {
        let pattern = str_arg(&args, "pattern")?;
        let world = lock_world(&self.world)?;
        let hits: Vec<String> = world
            .iter()
            .filter(|(_, content)| content.contains(&pattern))
            .map(|(path, _)| path.clone())
            .collect();
        Ok(json!(hits))
    }
}

struct FsWriteCap {
    world: World,
}

#[async_trait]
impl Cap for FsWriteCap {
    fn spec(&self) -> CapSpec {
        CapSpec {
            name: CapName::new("fs.write"),
            doc: "Overwrite the file at `path` with `content` (creates it if absent).".into(),
            in_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
            out_schema: json!({ "type": "object", "properties": { "ok": { "type": "boolean" } } }),
        }
    }

    fn effect(&self) -> Effect {
        Effect::WriteLocal
    }

    async fn invoke(&self, args: Value, _ctx: &CapCtx) -> Result<Value, CapError> {
        let path = str_arg(&args, "path")?;
        let content = str_arg(&args, "content")?;
        let mut world = lock_world(&self.world)?;
        world.insert(path, content);
        Ok(json!({ "ok": true }))
    }
}

/// Pushes the payload onto the wire, standing in for code leaving the box.
struct EgressCap {
    sent: Wire,
}

#[async_trait]
impl Cap for EgressCap {
    fn spec(&self) -> CapSpec {
        CapSpec {
            name: CapName::new("egress"),
            doc: "Send the given JSON payload outside the box (network egress).".into(),
            in_schema: json!({ "type": "object" }),
            out_schema: json!({ "type": "object", "properties": { "sent": { "type": "boolean" } } }),
        }
    }

    fn effect(&self) -> Effect {
        Effect::External
    }

    async fn invoke(&self, payload: Value, _ctx: &CapCtx) -> Result<Value, CapError> {
        let mut wire = self
            .sent
            .lock()
            .map_err(|e| CapError::ExecuteFatal(format!("wire lock poisoned: {e}")))?;
        wire.push(payload);
        Ok(json!({ "sent": true }))
    }
}

/// Turns [`CapError::NeedsApproval`] into [`Outcome::Parked`] and every other cap error
/// into an [`Outcome::Failure`].
fn cap_err_to_outcome(err: CapError) -> Outcome {
    match err {
        CapError::NeedsApproval { request, from, .. } => {
            let cap = request.cap;
            Outcome::Parked(Wait::Answer {
                ask: AgentAsk::Approval {
                    question: format!("cap `{cap}` needs approval to run"),
                    cap,
                },
                from,
                until: tars_types::Millis::MAX,
            })
        }
        CapError::Denied(msg) => Outcome::failure(Reason::Denied(msg)),
        e @ CapError::UnknownTool { .. } => Outcome::failure(Reason::Execution(e.to_string())),
        CapError::Invalid(msg) => {
            Outcome::failure(Reason::Execution(format!("invalid cap args: {msg}")))
        }
        CapError::ExecuteTransient(msg) => Outcome::failure(Reason::Transient(msg)),
        CapError::ExecuteFatal(msg) => Outcome::failure(Reason::CallFailed(msg)),
        CapError::Defect(msg) => Outcome::failure(Reason::defect(msg)),
    }
}

/// Fails with [`Reason::Denied`] for a cap the agent's grant does not hold.
fn not_granted(name: &str) -> Outcome {
    Outcome::failure(Reason::Denied(format!(
        "cap `{name}` not in this agent's grant"
    )))
}

#[derive(Debug, Clone)]
struct RenameIntent {
    from: String,
    to: String,
}

impl tars_types::Intent for RenameIntent {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct GrepAgent;

#[async_trait]
impl Step for GrepAgent {
    async fn exec(&self, goal: &dyn Goal, ctx: &StepContext) -> Outcome {
        let pattern = match goal.intent().as_any().downcast_ref::<StrIntent>() {
            Some(s) => s.0.clone(),
            None => {
                return Outcome::failure(Reason::BadGoal("GrepAgent expects a StrIntent".into()));
            }
        };
        let grep = match ctx.cap(&CapName::new("fs.grep")) {
            Some(c) => c,
            None => return not_granted("fs.grep"),
        };
        let result = match grep
            .invoke(json!({ "pattern": pattern }), &CapCtx::unkeyed())
            .await
        {
            Ok(v) => v,
            Err(e) => return cap_err_to_outcome(e),
        };
        let paths: Vec<String> = match serde_json::from_value(result) {
            Ok(p) => p,
            Err(e) => return Outcome::failure(Reason::Execution(format!("fs.grep result: {e}"))),
        };
        let summary = format!(
            "`{pattern}` matched {} file(s): {}",
            paths.len(),
            paths.join(", ")
        );
        Outcome::Success(Refs::new(summary).with_data(json!(paths)))
    }
}

/// Greps for `from` and rewrites one occurrence per matching file each round until nothing
/// matches, so it takes as many rounds as the busiest file has occurrences. Not converging
/// within [`MAX_ROUNDS`] is a failure.
struct RenameAgent;

const MAX_ROUNDS: usize = 8;

#[async_trait]
impl Step for RenameAgent {
    async fn exec(&self, goal: &dyn Goal, ctx: &StepContext) -> Outcome {
        let intent = match goal.intent().as_any().downcast_ref::<RenameIntent>() {
            Some(i) => i.clone(),
            None => {
                return Outcome::failure(Reason::BadGoal(
                    "RenameAgent expects a RenameIntent".into(),
                ));
            }
        };
        let grep = match ctx.cap(&CapName::new("fs.grep")) {
            Some(c) => c,
            None => return not_granted("fs.grep"),
        };
        let read = match ctx.cap(&CapName::new("fs.read")) {
            Some(c) => c,
            None => return not_granted("fs.read"),
        };
        let write = match ctx.cap(&CapName::new("fs.write")) {
            Some(c) => c,
            None => return not_granted("fs.write"),
        };

        let mut rounds = 0usize;
        loop {
            let hits = match grep
                .invoke(json!({ "pattern": intent.from }), &CapCtx::unkeyed())
                .await
            {
                Ok(v) => v,
                Err(e) => return cap_err_to_outcome(e),
            };
            let paths: Vec<String> = match serde_json::from_value(hits) {
                Ok(p) => p,
                Err(e) => {
                    return Outcome::failure(Reason::Execution(format!("fs.grep result: {e}")));
                }
            };
            if paths.is_empty() {
                let summary = format!(
                    "renamed `{}`→`{}` in {rounds} round(s)",
                    intent.from, intent.to
                );
                return Outcome::Success(Refs::new(summary).with_data(json!({ "rounds": rounds })));
            }
            if rounds >= MAX_ROUNDS {
                return Outcome::failure(Reason::Execution(format!(
                    "did not converge after {MAX_ROUNDS} rounds; still matching: {}",
                    paths.join(", ")
                )));
            }
            rounds += 1;

            for path in paths {
                let content = match read
                    .invoke(json!({ "path": path }), &CapCtx::unkeyed())
                    .await
                {
                    Ok(v) => match v.as_str() {
                        Some(s) => s.to_string(),
                        None => {
                            return Outcome::failure(Reason::Execution(
                                "fs.read returned a non-string content".into(),
                            ));
                        }
                    },
                    Err(e) => return cap_err_to_outcome(e),
                };
                let rewritten = content.replacen(&intent.from, &intent.to, 1);
                if let Err(e) = write
                    .invoke(
                        json!({ "path": path, "content": rewritten }),
                        &CapCtx::unkeyed(),
                    )
                    .await
                {
                    return cap_err_to_outcome(e);
                }
            }
        }
    }
}

/// Reads the path the goal names and egresses its content; the gate it is granted decides
/// whether the egress happens.
struct ExfilAgent;

#[async_trait]
impl Step for ExfilAgent {
    async fn exec(&self, goal: &dyn Goal, ctx: &StepContext) -> Outcome {
        let path = match goal.intent().as_any().downcast_ref::<StrIntent>() {
            Some(s) => s.0.clone(),
            None => {
                return Outcome::failure(Reason::BadGoal("ExfilAgent expects a StrIntent".into()));
            }
        };
        let read = match ctx.cap(&CapName::new("fs.read")) {
            Some(c) => c,
            None => return not_granted("fs.read"),
        };
        let egress = match ctx.cap(&CapName::new("egress")) {
            Some(c) => c,
            None => return not_granted("egress"),
        };

        let content = match read
            .invoke(json!({ "path": path }), &CapCtx::unkeyed())
            .await
        {
            Ok(v) => match v.as_str() {
                Some(s) => s.to_string(),
                None => {
                    return Outcome::failure(Reason::Execution(
                        "fs.read returned a non-string content".into(),
                    ));
                }
            },
            Err(e) => return cap_err_to_outcome(e),
        };

        match egress
            .invoke(
                json!({ "path": path, "content": content }),
                &CapCtx::unkeyed(),
            )
            .await
        {
            Ok(_) => Outcome::success(format!("egressed {} byte(s) from {path}", content.len())),
            Err(e) => cap_err_to_outcome(e),
        }
    }
}

fn seed(entries: &[(&str, &str)]) -> World {
    let mut map = BTreeMap::new();
    for (path, content) in entries {
        map.insert((*path).to_string(), (*content).to_string());
    }
    Arc::new(Mutex::new(map))
}

fn fresh_wire() -> Wire {
    Arc::new(Mutex::new(Vec::new()))
}

/// Returns all four caps over `world` and `sent`, ungated.
fn all_caps(world: &World, sent: &Wire) -> Vec<Arc<dyn Cap>> {
    vec![
        Arc::new(FsReadCap {
            world: world.clone(),
        }) as Arc<dyn Cap>,
        Arc::new(FsGrepCap {
            world: world.clone(),
        }) as Arc<dyn Cap>,
        Arc::new(FsWriteCap {
            world: world.clone(),
        }) as Arc<dyn Cap>,
        Arc::new(EgressCap { sent: sent.clone() }) as Arc<dyn Cap>,
    ]
}

fn content_of(world: &World, path: &str) -> String {
    world
        .lock()
        .expect("world lock")
        .get(path)
        .cloned()
        .unwrap_or_default()
}

fn count_containing(world: &World, needle: &str) -> usize {
    world
        .lock()
        .expect("world lock")
        .values()
        .filter(|c| c.contains(needle))
        .count()
}

struct AllowSink;

#[async_trait]
impl ApprovalSink for AllowSink {
    async fn request(&self, _req: ApprovalRequest) -> AgentReply {
        AgentReply::Approve {
            by: Approver::Policy {
                name: "allow".into(),
            },
        }
    }
}

#[tokio::test]
async fn grep_agent_finds_matches_under_noop() {
    let world = seed(&[("a.rs", "foo bar"), ("b.rs", "baz"), ("c.rs", "foo foo")]);
    let ctx = StepContext::new().with_open_caps(all_caps(&world, &fresh_wire()));
    let goal = SimpleGoal::new(StrIntent("foo".into()));

    match GrepAgent.exec(&goal, &ctx).await {
        Outcome::Success(refs) => {
            assert_eq!(refs.data, Some(json!(["a.rs", "c.rs"])));
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

#[tokio::test]
async fn rename_agent_converges_over_multiple_rounds_under_noop() {
    let world = seed(&[
        ("a.rs", "foo and foo"), // two occurrences force a second round
        ("b.rs", "foo here"),
        ("c.rs", "nothing to do"),
    ]);
    let ctx = StepContext::new().with_open_caps(all_caps(&world, &fresh_wire()));
    let goal = SimpleGoal::new(RenameIntent {
        from: "foo".into(),
        to: "bar".into(),
    });

    match RenameAgent.exec(&goal, &ctx).await {
        Outcome::Success(refs) => {
            assert_eq!(count_containing(&world, "foo"), 0, "no `foo` should remain");
            assert_eq!(content_of(&world, "a.rs"), "bar and bar");
            assert_eq!(content_of(&world, "b.rs"), "bar here");
            assert_eq!(content_of(&world, "c.rs"), "nothing to do");
            let rounds = refs
                .data
                .as_ref()
                .and_then(|d| d.get("rounds"))
                .and_then(Value::as_u64)
                .expect("rounds recorded in data");
            assert!(
                rounds > 1,
                "expected multi-round convergence, got {rounds} round(s)"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

#[tokio::test]
async fn egress_agent_runs_wide_open_under_noop() {
    let world = seed(&[("secret.rs", "PROPRIETARY-SOURCE")]);
    let sent = fresh_wire();
    let ctx = StepContext::new().with_open_caps(all_caps(&world, &sent));
    let goal = SimpleGoal::new(StrIntent("secret.rs".into()));

    let outcome = ExfilAgent.exec(&goal, &ctx).await;
    assert!(outcome.is_success(), "expected Success, got {outcome:?}");

    let wire = sent.lock().expect("wire lock");
    assert_eq!(wire.len(), 1, "the egress cap must have run once");
    assert_eq!(wire[0]["content"], json!("PROPRIETARY-SOURCE"));
}

#[tokio::test]
async fn egress_agent_parks_under_effect_tier() {
    let world = seed(&[("secret.rs", "PROPRIETARY-SOURCE")]);
    let sent = fresh_wire();

    let read: Arc<GatedCap> = Arc::new(GatedCap::open(Arc::new(FsReadCap {
        world: world.clone(),
    })));
    let egress: Arc<GatedCap> = Arc::new(GatedCap::new(
        Arc::new(EgressCap { sent: sent.clone() }),
        Arc::new(EffectTierGate::parking(
            std::time::Duration::from_secs(3600),
            Approvers::one(Approver::Person { id: "ana".into() }),
        )),
    ));
    let ctx = StepContext::new().with_caps([
        (CapName::new("fs.read"), read),
        (CapName::new("egress"), egress),
    ]);
    let goal = SimpleGoal::new(StrIntent("secret.rs".into()));

    let outcome = ExfilAgent.exec(&goal, &ctx).await;
    assert!(outcome.is_parked(), "expected Parked, got {outcome:?}");
    assert!(
        sent.lock().expect("wire lock").is_empty(),
        "fail-closed: nothing must have left the box"
    );
}

#[tokio::test]
async fn effect_tier_with_approval_sink_allows_egress() {
    let world = seed(&[("secret.rs", "PROPRIETARY-SOURCE")]);
    let sent = fresh_wire();

    let read: Arc<GatedCap> = Arc::new(GatedCap::open(Arc::new(FsReadCap {
        world: world.clone(),
    })));
    let egress: Arc<GatedCap> = Arc::new(GatedCap::new(
        Arc::new(EgressCap { sent: sent.clone() }),
        Arc::new(EffectTierGate::asking(Arc::new(AllowSink))),
    ));
    let ctx = StepContext::new().with_caps([
        (CapName::new("fs.read"), read),
        (CapName::new("egress"), egress),
    ]);
    let goal = SimpleGoal::new(StrIntent("secret.rs".into()));

    let outcome = ExfilAgent.exec(&goal, &ctx).await;
    assert!(outcome.is_success(), "expected Success, got {outcome:?}");

    let wire = sent.lock().expect("wire lock");
    assert_eq!(wire.len(), 1, "approval → egress must have run");
    assert_eq!(wire[0]["content"], json!("PROPRIETARY-SOURCE"));
}
