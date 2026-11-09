//! Wraps a plain function over one intent type as a [`Step`].
use std::marker::PhantomData;

use async_trait::async_trait;

use crate::StepContext;
use crate::goal::Goal;
use crate::intent::Intent;
use crate::outcome::{Outcome, Reason};
use crate::step::Step;

/// A `Step` whose body is `f`, run over the goal's intent downcast to `I`.
///
/// A goal whose intent is not an `I` fails with a [`Reason::Defect`]: the program handed it the wrong intent, and `f` does not run.
pub struct FnStep<I, F> {
    f: F,
    _intent: PhantomData<fn(&I)>,
}

impl<I, F> FnStep<I, F> {
    pub fn new(f: F) -> Self {
        Self {
            f,
            _intent: PhantomData,
        }
    }
}

#[async_trait]
impl<I, F> Step for FnStep<I, F>
where
    I: Intent + 'static,
    F: Fn(&I, &StepContext) -> Outcome + Send + Sync,
{
    async fn exec(&self, goal: &dyn Goal, ctx: &StepContext) -> Outcome {
        match goal.intent().as_any().downcast_ref::<I>() {
            Some(intent) => (self.f)(intent, ctx),
            None => Outcome::failure(Reason::defect(format!(
                "intent did not downcast to {}",
                std::any::type_name::<I>()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::SimpleGoal;
    use crate::intent::StrIntent;
    use crate::outcome::Refs;

    #[tokio::test]
    async fn runs_f_on_matching_intent() {
        let step =
            FnStep::new(|i: &StrIntent, _: &StepContext| Outcome::Success(Refs::new(i.0.clone())));
        let goal = SimpleGoal::new(StrIntent("echo me".into()));
        match step.exec(&goal, &StepContext::new()).await {
            Outcome::Success(refs) => assert_eq!(refs.summary, "echo me"),
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_intent_of_another_type_is_a_defect_the_model_never_sees() {
        #[derive(Debug)]
        struct OtherIntent;
        impl Intent for OtherIntent {
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
        let step = FnStep::new(|_: &StrIntent, _: &StepContext| Outcome::Success(Refs::new("x")));
        let goal = SimpleGoal::new(OtherIntent);
        match step.exec(&goal, &StepContext::new()).await {
            Outcome::Failure(reason @ Reason::Defect { .. }) => assert!(!reason.may_reach_model()),
            other => panic!("expected Defect, got {other:?}"),
        }
    }
}
