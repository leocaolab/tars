//! Defines [`StepContext`], what one run of a step is given: its granted caps, its
//! cancellation token, the identity its calls are attributed to, and its place in a
//! durable job.
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use crate::cap::{Cap, GatedCap};
use crate::requires::{Requires, Unmet};
use crate::{CapName, JobId, TaskId};

/// A source of the current time that can stand in for the system clock.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Clone)]
pub struct StepContext {
    task_id: Option<TaskId>,
    /// The job scope a `spawn` cap stamps its children into.
    job_scope: Option<JobId>,
    pub trajectory_id: Option<String>,
    /// The claim held by the runner that supervises this run; `None` when nothing does.
    claim: Option<crate::LeaseToken>,

    pub cancel: CancellationToken,
    /// The identity provider calls made under this context are billed and traced to;
    /// [`call_context`](Self::call_context) refuses when it is `None`.
    pub request_context: Option<crate::Caller>,
    caps: Arc<BTreeMap<CapName, Arc<GatedCap>>>,
}

impl Default for StepContext {
    fn default() -> Self {
        Self {
            cancel: CancellationToken::new(),
            trajectory_id: None,
            task_id: None,
            request_context: None,
            job_scope: None,
            caps: Arc::new(BTreeMap::new()),
            claim: None,
        }
    }
}

impl StepContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_trajectory_id(mut self, id: impl Into<String>) -> Self {
        self.trajectory_id = Some(id.into());
        self
    }

    /// Sets the identity this run's provider calls are attributed to.
    pub fn with_request_context(mut self, rc: crate::Caller) -> Self {
        self.request_context = Some(rc);
        self
    }

    /// Returns the identity for one provider call under this context, with accumulators of
    /// its own ([`Caller::for_call`](crate::Caller::for_call)).
    ///
    /// # Errors
    ///
    /// Returns [`Reason::Defect`](crate::Reason::Defect) when the context carries no
    /// identity: a call is never attributed to a made-up tenant.
    pub fn call_context(&self) -> Result<crate::Caller, crate::Reason> {
        self.request_context
            .as_ref()
            .map(crate::Caller::for_call)
            .ok_or_else(|| {
                crate::Reason::defect(
                    "a provider call under a StepContext that carries no Caller: \
                     no tenant to attribute it to",
                )
            })
    }

    /// Sets the identity when `rc` is `Some`; `None` leaves the current one in place.
    pub fn with_optional_request_context(mut self, rc: Option<crate::Caller>) -> Self {
        if let Some(rc) = rc {
            self.request_context = Some(rc);
        }
        self
    }

    /// Marks that a scheduler above this step, holding the task under `claim`, records how
    /// it ends. The step's own durable store writes speak for that claim, so once the task is no
    /// longer held under it they are refused while the task can still run.
    pub fn with_supervisor(mut self, claim: crate::LeaseToken) -> Self {
        self.claim = Some(claim);
        self
    }

    /// Returns whether a scheduler above this step records how it ends.
    pub fn is_supervised(&self) -> bool {
        self.claim.is_some()
    }

    /// Returns the claim the supervising scheduler holds the task under, or `None` when
    /// nothing supervises this run.
    pub fn claim(&self) -> Option<crate::LeaseToken> {
        self.claim
    }

    /// Sets the durable task this run belongs to.
    pub fn with_task_id(mut self, id: TaskId) -> Self {
        self.task_id = Some(id);
        self
    }

    /// Returns the durable task id, or `None` outside a durable run.
    pub fn task_id(&self) -> Option<&TaskId> {
        self.task_id.as_ref()
    }
    /// Sets the job a `spawn` cap stamps this run's children into.
    pub fn with_job_scope(mut self, job_id: JobId) -> Self {
        self.job_scope = Some(job_id);
        self
    }

    /// Returns the job this run belongs to, or `None` outside a durable run.
    pub fn job_scope(&self) -> Option<&JobId> {
        self.job_scope.as_ref()
    }

    /// Grants the caps the step may invoke, replacing any earlier grant.
    pub fn with_caps(mut self, caps: impl IntoIterator<Item = (CapName, Arc<GatedCap>)>) -> Self {
        self.caps = Arc::new(caps.into_iter().collect());
        self
    }

    /// Grants raw caps behind [`NoopGate`](crate::NoopGate), so every effect runs without
    /// approval; a fail-closed grant wraps them in
    /// [`EffectTierGate`](crate::EffectTierGate) and uses [`with_caps`](Self::with_caps).
    pub fn with_open_caps(self, caps: impl IntoIterator<Item = Arc<dyn Cap>>) -> Self {
        self.with_caps(caps.into_iter().map(|cap| {
            let gated = GatedCap::open(cap);
            (gated.name().clone(), Arc::new(gated))
        }))
    }

    /// Returns the granted cap named `name`, or `None` when it is not granted.
    pub fn cap(&self, name: &CapName) -> Option<&GatedCap> {
        self.caps.get(name).map(|c| c.as_ref())
    }

    /// Iterates the granted caps sorted by name, so the tool list a model is offered is
    /// byte-stable across processes and stays prompt-cacheable.
    pub fn caps_iter(&self) -> impl Iterator<Item = &GatedCap> {
        self.caps.values().map(|c| c.as_ref())
    }

    /// Checks that this grant holds every non-optional cap `req` names, each at or above
    /// the declared tier.
    ///
    /// # Errors
    ///
    /// Returns [`Unmet`] naming the missing and under-granted caps and everything that is
    /// granted.
    pub fn provide(&self, req: &Requires) -> Result<(), Unmet> {
        let mut missing = Vec::new();
        let mut under_granted = Vec::new();

        for need in &req.caps {
            match self.cap(&need.name) {
                None if !need.optional => missing.push(need.clone()),
                None => {}
                Some(held) if held.effect() < need.effect => {
                    under_granted.push((need.clone(), held.effect()))
                }
                Some(_) => {}
            }
        }

        if missing.is_empty() && under_granted.is_empty() {
            return Ok(());
        }
        Err(Unmet {
            missing,
            under_granted,
            granted: self.caps.keys().cloned().collect(),
        })
    }

    /// Returns the part of this grant that `req` names, which is what a parent hands a
    /// child: never more than the parent holds. A need the parent lacks is absent from
    /// the result; the child's own [`provide`](Self::provide) refuses it.
    pub fn attenuated(&self, req: &Requires) -> BTreeMap<CapName, Arc<GatedCap>> {
        req.caps
            .iter()
            .filter_map(|need| {
                self.caps
                    .get(&need.name)
                    .map(|held| (need.name.clone(), held.clone()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requires::CapReq;

    #[tokio::test]
    async fn provide_accepts_a_satisfying_grant() {
        let ctx = ctx_granting(&[("fs.read", Effect::Read), ("bash.run", Effect::External)]);
        let req = Requires::of([("fs.read", Effect::Read), ("bash.run", Effect::External)]);
        assert!(ctx.provide(&req).is_ok());
    }

    #[tokio::test]
    async fn provide_reports_the_missing_cap_and_what_was_granted() {
        let ctx = ctx_granting(&[("fs.read", Effect::Read), ("git.diff", Effect::Read)]);
        let req = Requires::of([("github.post_comment", Effect::External)]);

        let unmet = ctx
            .provide(&req)
            .expect_err("the grant cannot satisfy this");
        assert_eq!(
            unmet.missing,
            vec![CapReq::new(
                CapName::new("github.post_comment"),
                Effect::External
            )]
        );
        assert_eq!(
            unmet.granted,
            vec![CapName::new("fs.read"), CapName::new("git.diff")]
        );
        let msg = unmet.to_string();
        assert!(
            msg.contains("github.post_comment") && msg.contains("fs.read"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn provide_rejects_a_cap_granted_below_the_declared_tier() {
        let ctx = ctx_granting(&[("net.fetch", Effect::Read)]);
        let req = Requires::of([("net.fetch", Effect::External)]);

        let unmet = ctx
            .provide(&req)
            .expect_err("granted Read, declared External");
        assert!(
            unmet.missing.is_empty(),
            "it IS granted — just not strongly enough"
        );
        assert_eq!(
            unmet.under_granted,
            vec![(
                CapReq::new(CapName::new("net.fetch"), Effect::External),
                Effect::Read
            )]
        );
    }

    #[tokio::test]
    async fn provide_ignores_a_missing_optional_cap() {
        let ctx = ctx_granting(&[("fs.read", Effect::Read)]);
        let req = Requires::none()
            .and(CapReq::new(CapName::new("nice.to.have"), Effect::Read).optional());
        assert!(ctx.provide(&req).is_ok());
    }

    #[tokio::test]
    async fn attenuated_hands_down_a_subset_never_a_superset() {
        let parent = ctx_granting(&[
            ("fs.read", Effect::Read),
            ("fs.write_file", Effect::WriteLocal),
            ("bash.run", Effect::External),
        ]);
        let child_needs = Requires::of([("fs.read", Effect::Read)]);

        let child_grant = parent.attenuated(&child_needs);
        assert_eq!(
            child_grant.keys().cloned().collect::<Vec<_>>(),
            vec![CapName::new("fs.read")]
        );
        assert!(
            !child_grant.contains_key(&CapName::new("bash.run")),
            "egress must NOT leak down to the child"
        );

        let greedy = Requires::of([
            ("bash.run", Effect::External),
            ("k8s.delete", Effect::External),
        ]);
        let g = parent.attenuated(&greedy);
        assert!(g.contains_key(&CapName::new("bash.run")));
        assert!(
            !g.contains_key(&CapName::new("k8s.delete")),
            "a parent cannot pass on what it never held"
        );
    }

    fn ctx_granting(caps: &[(&str, Effect)]) -> StepContext {
        let grant: BTreeMap<CapName, Arc<GatedCap>> = caps
            .iter()
            .map(|(name, effect)| {
                let cap: Arc<dyn Cap> = Arc::new(TierCap {
                    name: name.to_string(),
                    effect: *effect,
                });
                (name.parse().unwrap(), Arc::new(GatedCap::open(cap)))
            })
            .collect();
        StepContext::new().with_caps(grant)
    }

    use crate::cap::{CapCtx, CapError, CapSpec, Effect, EffectTierGate};
    use async_trait::async_trait;
    use serde_json::{Value, json};

    fn dummy_ctx() -> CapCtx {
        CapCtx::unkeyed()
    }

    struct TierCap {
        name: String,
        effect: Effect,
    }

    #[async_trait]
    impl Cap for TierCap {
        fn spec(&self) -> CapSpec {
            CapSpec {
                name: CapName::try_from(self.name.clone()).unwrap(),
                doc: "tier fixture".into(),
                in_schema: json!({ "type": "object" }),
                out_schema: json!({ "type": "object" }),
            }
        }
        fn effect(&self) -> Effect {
            self.effect
        }
        async fn invoke(&self, args: Value, _ctx: &CapCtx) -> Result<Value, CapError> {
            Ok(args)
        }
    }

    struct EchoCap {
        name: &'static str,
        effect: Effect,
    }

    #[async_trait]
    impl Cap for EchoCap {
        fn spec(&self) -> CapSpec {
            CapSpec {
                name: CapName::new(self.name),
                doc: "echo cap".into(),
                in_schema: json!({}),
                out_schema: json!({}),
            }
        }
        fn effect(&self) -> Effect {
            self.effect
        }
        async fn invoke(&self, args: Value, _ctx: &CapCtx) -> Result<Value, CapError> {
            Ok(args)
        }
    }

    #[tokio::test]
    async fn with_caps_grants_an_invocable_handle() {
        let gated = Arc::new(GatedCap::open(Arc::new(EchoCap {
            name: "fs.read",
            effect: Effect::Read,
        })));
        let ctx = StepContext::new().with_caps([(CapName::new("fs.read"), gated)]);
        let handle = ctx
            .cap(&CapName::new("fs.read"))
            .expect("granted cap present");
        let out = handle
            .invoke(json!({ "path": "x" }), &dummy_ctx())
            .await
            .expect("read runs");
        assert_eq!(out, json!({ "path": "x" }));
    }

    #[test]
    fn empty_grant_by_default() {
        assert!(StepContext::new().cap(&CapName::new("fs.read")).is_none());
    }

    #[test]
    fn caps_iter_yields_sorted_by_name() {
        let mk = |name: &'static str| -> (CapName, Arc<GatedCap>) {
            let gated = Arc::new(GatedCap::open(Arc::new(EchoCap {
                name,
                effect: Effect::Read,
            })));
            (CapName::new(name), gated)
        };
        let ctx = StepContext::new().with_caps([
            mk("git.push"),
            mk("fs.read"),
            mk("net.fetch"),
            mk("fs.write"),
        ]);
        let order: Vec<&str> = ctx.caps_iter().map(|c| c.name().as_str()).collect();
        assert_eq!(
            order,
            vec!["fs.read", "fs.write", "git.push", "net.fetch"],
            "caps_iter must be sorted by name deterministically"
        );
    }

    #[tokio::test]
    async fn with_open_caps_grants_fully_open() {
        let ctx = StepContext::new().with_open_caps([Arc::new(EchoCap {
            name: "net.fetch",
            effect: Effect::External,
        }) as Arc<dyn Cap>]);
        let handle = ctx
            .cap(&CapName::new("net.fetch"))
            .expect("granted cap present");
        handle
            .invoke(json!({ "url": "x" }), &dummy_ctx())
            .await
            .expect("open egress runs");
    }

    #[tokio::test]
    async fn effect_tier_grant_resolves_and_gates() {
        let cap: Arc<dyn Cap> = Arc::new(EchoCap {
            name: "net.fetch",
            effect: Effect::External,
        });
        let gated = Arc::new(GatedCap::new(
            cap,
            Arc::new(EffectTierGate::parking(
                std::time::Duration::from_secs(3600),
                crate::Approvers::one(crate::Approver::Person { id: "ana".into() }),
            )),
        ));
        let ctx = StepContext::new().with_caps([(CapName::new("net.fetch"), gated)]);
        let handle = ctx
            .cap(&CapName::new("net.fetch"))
            .expect("granted cap present");
        match handle.invoke(json!({}), &dummy_ctx()).await {
            Err(CapError::NeedsApproval { .. }) => {}
            other => panic!("expected NeedsApproval under EffectTierGate, got {other:?}"),
        }
    }

    #[test]
    fn a_provider_call_with_no_identity_never_reaches_the_model() {
        let err = StepContext::new()
            .call_context()
            .expect_err("a context built without a Caller carries no tenant");
        assert!(!err.may_reach_model(), "{err:?}");
    }
}
