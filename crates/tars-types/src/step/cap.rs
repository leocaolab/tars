//! Defines [`Cap`], one thing a step can do to the world, and the gate every invocation
//! passes through.
//!
//! A step reaches caps only as [`GatedCap`]s from its [`StepContext`](crate::StepContext), and a
//! `GatedCap` asks its [`CapGate`] before it runs the cap. [`GatedCap::invoke_approved`] is the
//! one path past a gate's `NeedsApproval`, and nothing in its signature proves an approval: it is
//! meant for the runtime, after a reply approved the call, but a step holding the cap can call it
//! too.
//! Under [`EffectTierGate`], `Read` and `WriteLocal` run and `External` runs only on a
//! human's approval; a parking gate returns [`CapError::NeedsApproval`], which the runtime
//! turns into a durable park that lasts as long as the gate says a human has to answer.
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::CapName;
use crate::permission::{Permissions, PolicyVerdict};

/// A cap's blast radius, ordered `Read < WriteLocal < External` and declared once per cap,
/// whatever the arguments. The gate trusts it completely and cannot catch a cap that
/// claims `Read` but writes, so a cap declares its worst case.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Observes the world and changes nothing.
    Read,
    /// Changes local, confined state such as the worktree.
    ///
    /// Only a cap whose sandbox (gVisor, a network namespace) prevents egress may declare
    /// it: a `bash.run` in a permissive container is `External`, and declaring it lower
    /// lets it past the gate.
    WriteLocal,
    /// Can send data outside the box.
    External,
}

/// The whole description of a cap that the deciding logic gets; a model reads `doc` to
/// choose which cap to call.
#[derive(Clone, Debug)]
pub struct CapSpec {
    /// The key that policies and grants use, written `category.action` (`fs.read`,
    /// `git.push`).
    pub name: CapName,
    /// What the cap does and when it applies, written for the model that chooses it.
    pub doc: String,
    pub in_schema: Value,
    pub out_schema: Value,
}

impl CapSpec {
    /// Returns this cap as the tool spec a provider is given.
    ///
    /// The schema is `loose`, because the cap's own decode, not the provider, decides
    /// whether arguments are right.
    ///
    /// # Errors
    ///
    /// Returns [`CapSpecError`], naming the cap, when the name is empty or whitespace or the
    /// input schema's root is not an object; the cap is refused rather than silently left out
    /// of the tool list.
    pub fn to_tool_spec(&self) -> Result<crate::ToolSpec, CapSpecError> {
        let refused = |problem: CapSpecProblem| CapSpecError {
            cap: self.name.clone(),
            problem,
        };
        let root = crate::schema_root(self.in_schema.clone())
            .map_err(|e| refused(CapSpecProblem::SchemaRoot(e)))?;
        crate::ToolSpec::new(
            self.name.as_str(),
            &self.doc,
            crate::JsonSchema::loose(root),
        )
        .map_err(|e| refused(CapSpecProblem::BlankName(e)))
    }
}

/// A cap that cannot be offered to the model as a tool.
#[derive(Debug, thiserror::Error)]
#[error("capability `{cap}` cannot be offered as a tool")]
pub struct CapSpecError {
    pub cap: CapName,
    #[source]
    pub problem: CapSpecProblem,
}

/// Why a cap's spec does not make a tool spec.
#[derive(Debug, thiserror::Error)]
pub enum CapSpecProblem {
    #[error(transparent)]
    SchemaRoot(crate::providers::schema::SchemaRootError),
    #[error(transparent)]
    BlankName(crate::providers::tools::BlankToolNameError),
}

/// The name of one logical cap call: the same string on every attempt and resume of that
/// call, a different one for any other call.
///
/// It is never empty: an empty key is one key shared by every caller that sends it, and a
/// remote system honouring it would collapse all their calls into one. Where no logical
/// call stands behind an invocation, [`CapCtx::idempotency_key`] is `None`. Only the
/// constructors build one:
///
/// ```compile_fail
/// let _ = tars_types::IdempotencyKey(String::new());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

/// Refuses an empty idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "an idempotency key cannot be empty: every caller sending it would be deduped into one call"
)]
pub struct EmptyIdempotencyKey;

impl IdempotencyKey {
    /// Returns a key the caller derived from the intent of the call.
    ///
    /// A provider's tool-call id is not such a key: a retried turn gets a fresh one, so it
    /// names an attempt.
    ///
    /// # Errors
    ///
    /// Returns [`EmptyIdempotencyKey`] for the empty string.
    pub fn new(key: impl Into<String>) -> Result<Self, EmptyIdempotencyKey> {
        let key = key.into();
        if key.is_empty() {
            return Err(EmptyIdempotencyKey);
        }
        Ok(Self(key))
    }

    /// Returns the engine's key for a call site within a running task, `task#site`.
    ///
    /// The site alone would give two sibling tasks the same key, and the task alone would
    /// give every call in a task the same key. It is recomputed rather than stored, so a
    /// fresh process derives the same string.
    pub fn at_site(task: &str, site: &impl std::fmt::Display) -> Self {
        Self(format!("{task}#{site}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What one cap invocation is told by its caller.
///
/// # Exactly-once is the cap author's job, and nothing enforces it
///
/// The engine hands down a stable key, and the cap forwards it to the system that can
/// dedupe on it (an `Idempotency-Key` header, a unique constraint, an upsert); only that
/// system can close a crash between the effect and the record. A cap that ignores
/// [`idempotency_key`](Self::idempotency_key) looks like one that honours it until a
/// resume charges a card twice.
#[derive(Clone, Debug)]
pub struct CapCtx {
    /// The key of the logical call behind this invocation, the same on every attempt and
    /// resume ([`IdempotencyKey::at_site`]). Two tasks that must perform an action once
    /// between them need a lock, not a key.
    ///
    /// `None` when no durable call stands behind the invocation (a debugger, a console, a
    /// test): the cap then forwards no key and never makes one up, since a made-up key
    /// promises an exactly-once nobody keeps.
    pub idempotency_key: Option<IdempotencyKey>,
    /// Why the agent chose this call; it is shown to the human who approves it.
    pub rationale: Option<String>,
    /// The identity of the task making the call; provider calls the cap makes are
    /// attributed to it. `None` when no task stands behind the call.
    pub request_context: Option<crate::Caller>,
}

impl CapCtx {
    /// Returns the context of a durable invocation; `key` must be the same every time
    /// this logical call is made.
    pub fn keyed(key: IdempotencyKey) -> Self {
        Self::with_key(Some(key))
    }

    /// Returns the context of a direct invocation no durable call stands behind.
    pub fn unkeyed() -> Self {
        Self::with_key(None)
    }

    fn with_key(idempotency_key: Option<IdempotencyKey>) -> Self {
        Self {
            idempotency_key,
            rationale: None,
            request_context: None,
        }
    }

    /// Sets the identity of the task making this call.
    pub fn on_behalf_of(mut self, task: Option<crate::Caller>) -> Self {
        self.request_context = task;
        self
    }

    /// Sets why the agent chose this call.
    pub fn with_rationale(mut self, why: impl Into<String>) -> Self {
        self.rationale = Some(why.into());
        self
    }
}

/// The ungated cap an author writes; [`GatedCap`] adds the permission check, so `invoke`
/// does none.
#[async_trait]
pub trait Cap: Send + Sync {
    fn spec(&self) -> CapSpec;
    fn effect(&self) -> Effect;
    /// Runs the cap. A step reaches it only through [`GatedCap::invoke`].
    ///
    /// An `External` cap that does not forward `ctx`'s idempotency key is not
    /// exactly-once, and nothing above notices (see [`CapCtx`]).
    async fn invoke(&self, args: Value, ctx: &CapCtx) -> Result<Value, CapError>;
}

/// Why a cap invocation did not produce a value.
#[derive(Debug, thiserror::Error)]
pub enum CapError {
    /// A policy or a human refused the call.
    #[error("denied: {0}")]
    Denied(String),
    /// The call needs approval from one of `from`, who cannot be asked while the call waits;
    /// the cap did not run. The runtime parks the task, and a reply from one of `from`
    /// resumes it. One who has not answered within `answer_within` of the first park has not
    /// approved it.
    #[error("needs approval for cap `{}`", .request.cap)]
    NeedsApproval {
        request: Box<ApprovalRequest>,
        answer_within: std::time::Duration,
        from: crate::Approvers,
    },
    /// The call named a tool nothing is registered under; the model can name another.
    #[error("no tool registered with name `{name}`")]
    UnknownTool { name: String },
    /// The arguments were wrong; the model can correct them.
    #[error("invalid args: {0}")]
    Invalid(InvalidArgs),
    /// The cap failed in a way it knows can pass on another try: a lock another
    /// connection held, a dropped connection. It is the only failure retried inside the
    /// call, and only the cap can say it.
    #[error("transient execute error: {0}")]
    ExecuteTransient(String),
    /// The cap failed and did not call the failure transient. Another try is not known to
    /// pass, and the call may already have had its effect, so it is not retried; its words
    /// reach the model, which may ask for something else.
    #[error("fatal execute error: {0}")]
    ExecuteFatal(String),
}

/// How a call's arguments were wrong.
#[derive(Debug, thiserror::Error)]
pub enum InvalidArgs {
    /// The arguments did not decode into the cap's input; `args` is what was sent, so the
    /// words name it even where the caller's raw text is not shown.
    #[error("{cap}: bad arguments ({why}): {args}")]
    Undecodable {
        cap: String,
        why: String,
        args: Value,
    },
    /// The cap rejected the arguments, in its own words.
    #[error("{0}")]
    Rejected(String),
}

impl CapError {
    /// Returns why the call failed, with the call's `args` as the raw text of a call to an
    /// unknown tool or with invalid arguments, so the model sees what it sent.
    ///
    /// A caller that can park for an approval matches [`NeedsApproval`](Self::NeedsApproval)
    /// first. Reaching here with one means the call asked for approval where nobody can be
    /// asked, which is our wiring's mistake, not something a model could change.
    pub fn failure(self, args: &Value) -> crate::Reason {
        use crate::Reason;
        match self {
            Self::NeedsApproval { request, .. } => Reason::defect(format!(
                "cap `{}` needs approval here, where nobody can be asked for it",
                request.cap
            )),
            Self::Denied(m) => Reason::Denied(m),
            e @ Self::UnknownTool { .. } => Reason::invalid_input_truncated(
                &args.to_string(),
                e.to_string(),
                crate::RECORD_RAW_MAX,
            ),
            Self::Invalid(why) => Reason::invalid_input_truncated(
                &args.to_string(),
                why.to_string(),
                crate::RECORD_RAW_MAX,
            ),
            Self::ExecuteTransient(m) => Reason::Transient(m),
            Self::ExecuteFatal(m) => Reason::CallFailed(m),
        }
    }
}

/// What a human is shown when an invocation needs approval.
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub cap: CapName,
    pub effect: Effect,
    pub args: Value,
    pub rationale: Option<String>,
}

/// A channel that asks someone who can answer while the invocation waits (an agent, a
/// policy) whether it may run.
#[async_trait]
pub trait ApprovalSink: Send + Sync {
    /// Returns the reply to `req`, which says who gave it.
    async fn request(&self, req: ApprovalRequest) -> crate::AgentReply;
}

/// Decides whether a [`GatedCap`] may run its cap: fail-closed [`EffectTierGate`] for
/// untrusted callers, fully open [`NoopGate`] for a trusted autonomous one.
#[async_trait]
pub trait CapGate: Send + Sync {
    /// Admits or refuses one invocation, and returns who approved it, or `None` when it
    /// needed no approval.
    ///
    /// # Errors
    ///
    /// Returns [`CapError::Denied`] to block it and [`CapError::NeedsApproval`] to park it.
    async fn admit(
        &self,
        cap_name: &CapName,
        effect: Effect,
        args: &Value,
        ctx: &CapCtx,
    ) -> Result<Option<crate::Approver>, CapError>;
}

/// Admits every invocation, `External` included, for a trusted autonomous consumer such as
/// arc, where an approval prompt would stall the run.
pub struct NoopGate;

#[async_trait]
impl CapGate for NoopGate {
    async fn admit(
        &self,
        cap_name: &CapName,
        effect: Effect,
        _args: &Value,
        _ctx: &CapCtx,
    ) -> Result<Option<crate::Approver>, CapError> {
        tracing::debug!(
            cap = %cap_name,
            ?effect,
            decision = "allowed",
            gate = "noop",
            "cap gate: open, running",
        );
        Ok(None)
    }
}

/// Admits `Read` and `WriteLocal` and requires approval for `External`.
///
/// An optional [`Permissions`] policy is consulted first: `Deny` refuses whatever the
/// tier, `Ask` requires approval whatever the tier, and `Allow` leaves it to the tier.
/// Whoever builds the gate says where a call that needs approval goes: to an
/// [`ApprovalSink`] while the call waits, or into a park that one of the approvers it names
/// answers within a window it chooses, since only it knows who is asked and how soon they
/// look.
pub struct EffectTierGate {
    route: ApprovalRoute,
    policy: Option<Permissions>,
}

/// Where a call that needs approval goes.
enum ApprovalRoute {
    Ask(Arc<dyn ApprovalSink>),
    Park {
        answer_within: std::time::Duration,
        from: crate::Approvers,
    },
}

impl EffectTierGate {
    /// Returns a gate that asks `sink` while the call waits.
    pub fn asking(sink: Arc<dyn ApprovalSink>) -> Self {
        Self {
            route: ApprovalRoute::Ask(sink),
            policy: None,
        }
    }

    /// Returns a gate that refuses a call needing approval with
    /// [`CapError::NeedsApproval`], so the task parks; one of `from` has `answer_within` from
    /// the first park to answer before the call counts as unapproved.
    pub fn parking(answer_within: std::time::Duration, from: crate::Approvers) -> Self {
        Self {
            route: ApprovalRoute::Park {
                answer_within,
                from,
            },
            policy: None,
        }
    }

    /// Sets the per-cap policy consulted before the tier.
    pub fn with_policy(mut self, policy: Permissions) -> Self {
        self.policy = Some(policy);
        self
    }
}

#[async_trait]
impl CapGate for EffectTierGate {
    async fn admit(
        &self,
        cap_name: &CapName,
        effect: Effect,
        args: &Value,
        ctx: &CapCtx,
    ) -> Result<Option<crate::Approver>, CapError> {
        let name = cap_name;

        let mut needs_approval = effect == Effect::External;

        if let Some(policy) = &self.policy {
            match policy.decide(name.as_str()) {
                PolicyVerdict::Deny => {
                    tracing::info!(
                        cap = %name,
                        ?effect,
                        decision = "denied",
                        ran = false,
                        "cap gate: policy denied",
                    );
                    return Err(CapError::Denied(format!("policy denied cap `{name}`")));
                }
                PolicyVerdict::Ask => needs_approval = true,
                PolicyVerdict::Allow => {}
            }
        }

        if needs_approval {
            let req = ApprovalRequest {
                cap: name.clone(),
                effect,
                args: args.clone(),
                rationale: ctx.rationale.clone(),
            };
            match &self.route {
                ApprovalRoute::Ask(sink) => {
                    return match sink.request(req).await {
                        crate::AgentReply::Approve { by } => {
                            tracing::info!(cap = %name, ?effect, approver = ?by, decision = "approved", ran = true, "cap gate: approved");
                            Ok(Some(by))
                        }
                        crate::AgentReply::Deny { by, said } => {
                            tracing::info!(cap = %name, ?effect, approver = ?by, decision = "denied", ran = false, "cap gate: approval denied");
                            Err(CapError::Denied(format!(
                                "{by:?} refused cap `{name}`: {said}"
                            )))
                        }
                        crate::AgentReply::Answer { by, value } => Err(CapError::Denied(format!(
                            "{by:?} answered {value} where cap `{name}` asked for approval, \
                             which is not an approval"
                        ))),
                    };
                }
                ApprovalRoute::Park {
                    answer_within,
                    from,
                } => {
                    tracing::info!(
                        cap = %name,
                        ?effect,
                        decision = "parked",
                        ran = false,
                        "cap gate: egress needs approval, no sink → park",
                    );
                    return Err(CapError::NeedsApproval {
                        request: Box::new(req),
                        answer_within: *answer_within,
                        from: from.clone(),
                    });
                }
            }
        }

        tracing::debug!(
            cap = %name,
            ?effect,
            decision = "allowed",
            ran = true,
            "cap gate: running",
        );
        Ok(None)
    }
}

/// A cap behind its gate: the only form in which a step holds a cap, and the only
/// `invoke` it can reach.
pub struct GatedCap {
    inner: Arc<dyn Cap>,
    gate: Arc<dyn CapGate>,
    /// [`CapSpec::name`], kept so that a lookup or an admit does not clone the schemas
    /// `spec()` returns.
    name: CapName,
}

impl GatedCap {
    pub fn new(inner: Arc<dyn Cap>, gate: Arc<dyn CapGate>) -> Self {
        let name = inner.spec().name;
        Self { inner, gate, name }
    }

    /// Wraps a cap behind [`NoopGate`], so every effect runs without approval.
    pub fn open(inner: Arc<dyn Cap>) -> Self {
        Self::new(inner, Arc::new(NoopGate))
    }

    pub fn spec(&self) -> CapSpec {
        self.inner.spec()
    }

    pub fn effect(&self) -> Effect {
        self.inner.effect()
    }

    pub fn name(&self) -> &CapName {
        &self.name
    }

    /// Runs the cap if the gate admits it.
    ///
    /// # Errors
    ///
    /// Returns the gate's refusal (`Denied`, `NeedsApproval`) without running the cap, or
    /// the cap's own error.
    pub async fn invoke(&self, args: Value, ctx: &CapCtx) -> Result<Value, CapError> {
        self.invoke_with_approver(args, ctx).await.map(|(v, _)| v)
    }

    /// Runs the cap if the gate admits it, as [`invoke`](Self::invoke) does, and returns who
    /// approved the call, or `None` when it needed no approval.
    ///
    /// # Errors
    ///
    /// As [`invoke`](Self::invoke).
    pub async fn invoke_with_approver(
        &self,
        args: Value,
        ctx: &CapCtx,
    ) -> Result<(Value, Option<crate::Approver>), CapError> {
        let by = self
            .gate
            .admit(&self.name, self.inner.effect(), &args, ctx)
            .await?;
        Ok((self.inner.invoke(args, ctx).await?, by))
    }

    /// Runs a cap a human has already approved, as a resumed task does after
    /// [`invoke`](Self::invoke) parked it on `NeedsApproval`.
    ///
    /// The gate is asked again and its `NeedsApproval` counts as granted; a `Denied` still
    /// blocks, because approval never overrides a deny.
    pub async fn invoke_approved(&self, args: Value, ctx: &CapCtx) -> Result<Value, CapError> {
        match self
            .gate
            .admit(&self.name, self.inner.effect(), &args, ctx)
            .await
        {
            Ok(_) => {}
            Err(CapError::NeedsApproval { .. }) => {}
            Err(other) => return Err(other),
        }
        self.inner.invoke(args, ctx).await
    }
}

#[cfg(test)]
mod test_support {
    use super::*;

    /// How long a test's parking gate gives a human to answer.
    pub(super) const HOUR: std::time::Duration = std::time::Duration::from_secs(3600);

    /// Returns the one person a parking gate in these tests asks.
    pub(super) fn ana() -> crate::Approvers {
        crate::Approvers::one(crate::Approver::Person { id: "ana".into() })
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    pub(super) struct StubCap {
        effect: Effect,
        ran: Arc<AtomicUsize>,
    }

    impl StubCap {
        /// Returns a stub of `effect` and the counter of its runs.
        pub(super) fn make(effect: Effect) -> (Arc<dyn Cap>, Arc<AtomicUsize>) {
            let ran = Arc::new(AtomicUsize::new(0));
            let cap: Arc<dyn Cap> = Arc::new(StubCap {
                effect,
                ran: ran.clone(),
            });
            (cap, ran)
        }
    }

    #[async_trait]
    impl Cap for StubCap {
        fn spec(&self) -> CapSpec {
            CapSpec {
                name: CapName::new("test.cap"),
                doc: "a stub cap for gate tests".into(),
                in_schema: json!({ "type": "object" }),
                out_schema: json!({ "type": "object" }),
            }
        }

        fn effect(&self) -> Effect {
            self.effect
        }

        async fn invoke(&self, args: Value, _ctx: &CapCtx) -> Result<Value, CapError> {
            self.ran.fetch_add(1, Ordering::SeqCst);
            Ok(args)
        }
    }

    pub(super) fn ran(counter: &Arc<AtomicUsize>) -> usize {
        counter.load(Ordering::SeqCst)
    }

    /// A sink that always approves, or always refuses, as a policy named `fixed`.
    pub(super) struct FixedSink(pub(super) bool);

    #[async_trait]
    impl ApprovalSink for FixedSink {
        async fn request(&self, _req: ApprovalRequest) -> crate::AgentReply {
            let by = crate::Approver::Policy {
                name: "fixed".into(),
            };
            match self.0 {
                true => crate::AgentReply::Approve { by },
                false => crate::AgentReply::Deny {
                    by,
                    said: "no".into(),
                },
            }
        }
    }
}

#[cfg(test)]
mod idempotency_key_tests {
    use super::*;

    #[test]
    fn an_empty_key_is_refused() {
        assert_eq!(IdempotencyKey::new(""), Err(EmptyIdempotencyKey));
        assert_eq!(
            IdempotencyKey::new("call_1").map(|k| k.to_string()),
            Ok("call_1".into())
        );
    }

    #[test]
    fn a_site_key_names_the_task_and_the_site() {
        let key = IdempotencyKey::at_site("t1", &"cap:pay#2a");
        assert_eq!(key.as_str(), "t1#cap:pay#2a");
        assert_eq!(key, IdempotencyKey::at_site("t1", &"cap:pay#2a"));
        assert_ne!(key, IdempotencyKey::at_site("t2", &"cap:pay#2a"));
        assert_ne!(key, IdempotencyKey::at_site("t1", &"cap:pay#2b"));
    }

    #[test]
    fn a_direct_call_carries_no_key() {
        assert_eq!(CapCtx::unkeyed().idempotency_key, None);
    }
}

#[cfg(test)]
mod shared_tests {
    use super::test_support::{HOUR, StubCap, ana};
    use super::*;

    #[test]
    fn effect_tiers_are_ordered() {
        assert!(Effect::Read < Effect::WriteLocal);
        assert!(Effect::WriteLocal < Effect::External);
    }

    #[test]
    fn cap_is_object_safe() {
        let (cap, _ran) = StubCap::make(Effect::Read);
        let _boxed: Arc<dyn Cap> = cap;
    }

    #[test]
    fn cap_gate_is_object_safe() {
        let _open: Arc<dyn CapGate> = Arc::new(NoopGate);
        let _tier: Arc<dyn CapGate> = Arc::new(EffectTierGate::parking(HOUR, ana()));
    }
}

#[cfg(test)]
mod noop_gate_tests {
    use super::test_support::{StubCap, ran};
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn read_cap_runs() {
        let (cap, counter) = StubCap::make(Effect::Read);
        let out = GatedCap::new(cap, Arc::new(NoopGate))
            .invoke(json!({ "k": 1 }), &CapCtx::unkeyed())
            .await
            .expect("read runs");
        assert_eq!(out, json!({ "k": 1 }));
        assert_eq!(ran(&counter), 1);
    }

    #[tokio::test]
    async fn write_local_cap_runs() {
        let (cap, counter) = StubCap::make(Effect::WriteLocal);
        GatedCap::new(cap, Arc::new(NoopGate))
            .invoke(json!({}), &CapCtx::unkeyed())
            .await
            .expect("write_local runs");
        assert_eq!(ran(&counter), 1);
    }

    #[tokio::test]
    async fn external_cap_with_no_sink_runs_not_parked_not_denied() {
        let (cap, counter) = StubCap::make(Effect::External);
        let out = GatedCap::new(cap, Arc::new(NoopGate))
            .invoke(json!({ "payload": "secret" }), &CapCtx::unkeyed())
            .await
            .expect("external egress runs wide open");
        assert_eq!(out, json!({ "payload": "secret" }));
        assert_eq!(ran(&counter), 1, "external must run under NoopGate");
    }

    #[tokio::test]
    async fn gated_cap_open_uses_noop() {
        let (cap, counter) = StubCap::make(Effect::External);
        GatedCap::open(cap)
            .invoke(json!({}), &CapCtx::unkeyed())
            .await
            .expect("open egress runs");
        assert_eq!(ran(&counter), 1);
    }
}

#[cfg(test)]
mod effect_tier_gate_tests {
    use super::test_support::{FixedSink, HOUR, StubCap, ana, ran};
    use super::*;
    use serde_json::json;

    fn gated(cap: Arc<dyn Cap>, gate: EffectTierGate) -> GatedCap {
        GatedCap::new(cap, Arc::new(gate))
    }

    #[tokio::test]
    async fn read_cap_runs() {
        let (cap, counter) = StubCap::make(Effect::Read);
        let out = gated(cap, EffectTierGate::parking(HOUR, ana()))
            .invoke(json!({ "k": 1 }), &CapCtx::unkeyed())
            .await
            .expect("read runs");
        assert_eq!(out, json!({ "k": 1 }));
        assert_eq!(ran(&counter), 1);
    }

    #[tokio::test]
    async fn write_local_cap_runs() {
        let (cap, counter) = StubCap::make(Effect::WriteLocal);
        gated(cap, EffectTierGate::parking(HOUR, ana()))
            .invoke(json!({}), &CapCtx::unkeyed())
            .await
            .expect("write_local runs");
        assert_eq!(ran(&counter), 1);
    }

    #[tokio::test]
    async fn external_cap_with_no_sink_needs_approval_and_does_not_run() {
        let (cap, counter) = StubCap::make(Effect::External);
        match gated(cap, EffectTierGate::parking(HOUR, ana()))
            .invoke(json!({ "payload": "secret" }), &CapCtx::unkeyed())
            .await
        {
            Err(CapError::NeedsApproval {
                request: req,
                answer_within,
                ..
            }) => {
                assert_eq!(answer_within, HOUR, "the window the gate was built with");
                assert_eq!(req.cap, CapName::new("test.cap"));
                assert_eq!(req.effect, Effect::External);
                assert_eq!(req.args, json!({ "payload": "secret" }));
                assert_eq!(req.rationale, None);
            }
            other => panic!("expected NeedsApproval, got {other:?}"),
        }
        assert_eq!(ran(&counter), 0, "parked cap must not run");
    }

    #[tokio::test]
    async fn external_cap_with_allow_sink_runs() {
        let (cap, counter) = StubCap::make(Effect::External);
        let gate = EffectTierGate::asking(Arc::new(FixedSink(true)));
        gated(cap, gate)
            .invoke(json!({}), &CapCtx::unkeyed())
            .await
            .expect("approved egress runs");
        assert_eq!(ran(&counter), 1);
    }

    #[tokio::test]
    async fn external_cap_with_deny_sink_is_denied() {
        let (cap, counter) = StubCap::make(Effect::External);
        let gate = EffectTierGate::asking(Arc::new(FixedSink(false)));
        match gated(cap, gate).invoke(json!({}), &CapCtx::unkeyed()).await {
            Err(CapError::Denied(_)) => {}
            other => panic!("expected Denied, got {other:?}"),
        }
        assert_eq!(ran(&counter), 0, "denied cap must not run");
    }

    #[tokio::test]
    async fn policy_deny_beats_read_tier() {
        let (cap, counter) = StubCap::make(Effect::Read);
        let gate = EffectTierGate::parking(HOUR, ana())
            .with_policy(Permissions::allow_all().deny("test.cap"));
        match gated(cap, gate).invoke(json!({}), &CapCtx::unkeyed()).await {
            Err(CapError::Denied(_)) => {}
            other => panic!("expected Denied, got {other:?}"),
        }
        assert_eq!(ran(&counter), 0, "policy-denied cap must not run");
    }

    #[tokio::test]
    async fn policy_ask_on_read_with_allow_sink_runs() {
        let (cap, counter) = StubCap::make(Effect::Read);
        let gate = EffectTierGate::asking(Arc::new(FixedSink(true)))
            .with_policy(Permissions::allow_all().ask("test.cap"));
        gated(cap, gate)
            .invoke(json!({}), &CapCtx::unkeyed())
            .await
            .expect("ask+allow runs");
        assert_eq!(ran(&counter), 1);
    }

    #[tokio::test]
    async fn policy_ask_on_read_with_no_sink_needs_approval() {
        let (cap, counter) = StubCap::make(Effect::Read);
        let gate = EffectTierGate::parking(HOUR, ana())
            .with_policy(Permissions::allow_all().ask("test.cap"));
        match gated(cap, gate).invoke(json!({}), &CapCtx::unkeyed()).await {
            Err(CapError::NeedsApproval { .. }) => {}
            other => panic!("expected NeedsApproval, got {other:?}"),
        }
        assert_eq!(ran(&counter), 0);
    }

    #[tokio::test]
    async fn policy_allow_defers_to_tier_external_still_parks() {
        let (cap, counter) = StubCap::make(Effect::External);
        let gate = EffectTierGate::parking(HOUR, ana())
            .with_policy(Permissions::allow_all().allow("test.cap"));
        match gated(cap, gate).invoke(json!({}), &CapCtx::unkeyed()).await {
            Err(CapError::NeedsApproval { .. }) => {}
            other => panic!("expected NeedsApproval (tier still gates), got {other:?}"),
        }
        assert_eq!(ran(&counter), 0);
    }

    #[tokio::test]
    async fn invoke_approved_runs_an_external_no_sink_cap_the_park_would_block() {
        let (cap, counter) = StubCap::make(Effect::External);
        let g = gated(cap, EffectTierGate::parking(HOUR, ana()));
        let out = g
            .invoke_approved(json!({ "payload": "secret" }), &CapCtx::unkeyed())
            .await
            .expect("approved-out-of-band egress runs");
        assert_eq!(out, json!({ "payload": "secret" }));
        assert_eq!(ran(&counter), 1, "the approved cap runs exactly once");
    }

    #[tokio::test]
    async fn invoke_approved_still_honours_a_policy_deny() {
        let (cap, counter) = StubCap::make(Effect::External);
        let gate = EffectTierGate::parking(HOUR, ana())
            .with_policy(Permissions::allow_all().deny("test.cap"));
        match gated(cap, gate)
            .invoke_approved(json!({}), &CapCtx::unkeyed())
            .await
        {
            Err(CapError::Denied(_)) => {}
            other => panic!("expected Denied (approval never overrides a deny), got {other:?}"),
        }
        assert_eq!(ran(&counter), 0, "a denied cap never runs, approved or not");
    }
}

#[cfg(test)]
mod failure_tests {
    use serde_json::json;

    use super::*;
    use crate::Reason;

    fn fails(e: CapError) -> Reason {
        e.failure(&json!({ "path": "a.rs" }))
    }

    #[test]
    fn a_fatal_cap_failure_reaches_the_model_and_is_not_rerun_with_the_task() {
        let fatal = fails(CapError::ExecuteFatal("patch does not apply".into()));
        assert!(fatal.may_reach_model(), "{fatal:?}");
        assert!(!fatal.is_retryable(), "{fatal:?}");
    }

    #[test]
    fn a_transient_cap_failure_reaches_the_model_and_is_rerun_with_the_task() {
        let transient = fails(CapError::ExecuteTransient("database is locked".into()));
        assert!(transient.may_reach_model(), "{transient:?}");
        assert!(transient.is_retryable(), "{transient:?}");
    }

    #[test]
    fn a_denied_or_invalid_cap_call_reaches_the_model_that_made_it() {
        for e in [
            CapError::Denied("fs.write is outside the policy".into()),
            CapError::Invalid(InvalidArgs::Rejected("no `line`".into())),
        ] {
            let reason = fails(e);
            assert!(reason.may_reach_model(), "{reason:?}");
        }
    }

    #[test]
    fn an_invalid_call_carries_the_arguments_it_was_called_with() {
        let Reason::InvalidInput { raw, detail } =
            fails(CapError::Invalid(InvalidArgs::Rejected("no `line`".into())))
        else {
            panic!("an invalid call is Invalid");
        };
        assert_eq!(raw, r#"{"path":"a.rs"}"#);
        assert_eq!(detail, "no `line`");
    }

    #[test]
    fn an_unknown_tool_is_not_called_invalid_args() {
        let e = CapError::UnknownTool {
            name: "ghost".into(),
        };
        assert_eq!(e.to_string(), "no tool registered with name `ghost`");
    }

    #[test]
    fn a_call_to_an_unknown_tool_reaches_the_model_as_the_tool_named_it() {
        let Reason::InvalidInput { raw, detail } = fails(CapError::UnknownTool {
            name: "ghost".into(),
        }) else {
            panic!("a call to an unknown tool is InvalidInput");
        };
        assert_eq!(raw, r#"{"path":"a.rs"}"#);
        assert_eq!(detail, "no tool registered with name `ghost`");
    }

    fn undecodable() -> InvalidArgs {
        InvalidArgs::Undecodable {
            cap: "fs.read".into(),
            why: "missing field `path`".into(),
            args: json!({ "start": 7 }),
        }
    }

    #[test]
    fn undecodable_args_name_the_cap_the_reason_and_what_was_sent() {
        assert_eq!(
            CapError::Invalid(undecodable()).to_string(),
            r#"invalid args: fs.read: bad arguments (missing field `path`): {"start":7}"#
        );
    }

    #[test]
    fn undecodable_args_reach_the_model_in_the_cap_s_words() {
        let Reason::InvalidInput { raw, detail } = fails(CapError::Invalid(undecodable())) else {
            panic!("undecodable args are InvalidInput");
        };
        assert_eq!(raw, r#"{"path":"a.rs"}"#);
        assert_eq!(
            detail,
            r#"fs.read: bad arguments (missing field `path`): {"start":7}"#
        );
    }

    /// A caller that can park matches `NeedsApproval` before asking for a failure; one that
    /// reaches here had nobody to ask, which is our wiring's mistake.
    #[test]
    fn a_call_that_needs_approval_where_nobody_can_be_asked_is_ours() {
        let req = ApprovalRequest {
            cap: CapName::new("bash"),
            effect: Effect::External,
            args: json!({}),
            rationale: None,
        };
        let reason = CapError::NeedsApproval {
            request: Box::new(req),
            answer_within: std::time::Duration::from_secs(60),
            from: crate::Approvers::one(crate::Approver::Person { id: "ana".into() }),
        }
        .failure(&json!({}));
        assert!(!reason.may_reach_model(), "{reason:?}");
        assert!(reason.to_string().contains("bash"), "{reason}");
    }
}
