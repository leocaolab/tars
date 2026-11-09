//! Defines [`Goal`], the target of one pursuit: an [`Intent`] saying what to do and an
//! [`Acceptance`] saying who judges it done. One step pursues many goals, and one goal is
//! pursued over many contexts.
use crate::AgentKey;
use crate::intent::Intent;

/// The target a [`Step`](crate::Step) pursues.
pub trait Goal: Send + Sync {
    fn intent(&self) -> &dyn Intent;
    fn acceptance(&self) -> &Acceptance;

    /// Returns this goal's intent as JSON, or `None` when the intent is not a
    /// [`JsonIntent`](crate::JsonIntent).
    fn json(&self) -> Option<&serde_json::Value> {
        self.intent()
            .as_any()
            .downcast_ref::<crate::JsonIntent>()
            .map(|j| &j.0)
    }
}

// Fails to build if `Goal` stops being object-safe; every step takes `&dyn Goal`, which is
// why the generic `parse` lives on `GoalExt`.
const _: Option<&dyn Goal> = None;

/// Decodes a goal into a caller's own type; blanket-implemented, so it works on
/// `&dyn Goal` too.
pub trait GoalExt {
    /// Decodes this goal's JSON intent into `T`.
    ///
    /// # Errors
    ///
    /// Returns [`Reason::Defect`](crate::Reason::Defect) when the intent is not JSON or does
    /// not decode; `raw` holds the intent it was handed. A goal is written by a program or by
    /// a model, and the parse cannot tell which, so it is not shown to a model.
    fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, crate::Reason>;
}

impl<G: Goal + ?Sized> GoalExt for G {
    fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, crate::Reason> {
        let raw = self.json().ok_or_else(|| crate::Reason::Defect {
            raw: Some(format!("{:?}", self.intent())),
            detail: "this goal's intent is not JSON".into(),
        })?;
        serde_json::from_value(raw.clone()).map_err(|e| crate::Reason::Defect {
            raw: Some(raw.to_string()),
            detail: format!(
                "goal does not decode into {}: {e}",
                std::any::type_name::<T>()
            ),
        })
    }
}

/// Says who judges a [`Goal`] done.
///
/// The judge is named by its registry key rather than held as an `Arc<dyn Step>`: the
/// durable engine spawns steps by key, and a key survives on the task row as its `judge`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Acceptance {
    /// The registry key of a separate step that judges the work.
    By(AgentKey),
    /// The acting step judges itself; fit only when the outcome is self-evident.
    SelfJudged,
}

impl Acceptance {
    /// Returns whether a step other than the acting one judges the work.
    pub fn is_independent(&self) -> bool {
        matches!(self, Self::By(_))
    }

    /// Returns the judge's registry key, or `None` when the step judges itself.
    pub fn judge_key(&self) -> Option<&AgentKey> {
        match self {
            Self::By(k) => Some(k),
            Self::SelfJudged => None,
        }
    }
}

/// What a judge returns in its outcome's [`Refs::data`](crate::Refs::data).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Judgement {
    pub passed: bool,
    /// A score for consumers that rank rather than gate; `None` from a pass/fail judge,
    /// which measured no number.
    pub score: Option<f64>,
    /// Why the judge decided so; when the work did not pass, this goes back to the actor.
    pub feedback: String,
}

/// Any [`Intent`] paired with an [`Acceptance`].
pub struct SimpleGoal {
    intent: Box<dyn Intent>,
    acceptance: Acceptance,
}

impl SimpleGoal {
    /// Returns a self-judged goal; [`judged_by`](Self::judged_by) names a judge.
    pub fn new(intent: impl Intent + 'static) -> Self {
        Self {
            intent: Box::new(intent),
            acceptance: Acceptance::SelfJudged,
        }
    }

    /// Has the step registered under `judge_key` judge the work.
    pub fn judged_by(mut self, judge_key: AgentKey) -> Self {
        self.acceptance = Acceptance::By(judge_key);
        self
    }

    /// Rebuilds a goal from its parts, as the engine does when it rehydrates a task.
    pub fn restored(intent: Box<dyn Intent>, acceptance: Acceptance) -> Self {
        Self { intent, acceptance }
    }
}

impl Goal for SimpleGoal {
    fn intent(&self) -> &dyn Intent {
        &*self.intent
    }
    fn acceptance(&self) -> &Acceptance {
        &self.acceptance
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_goal_that_does_not_decode_never_reaches_the_model() {
        let goal = SimpleGoal::new(crate::JsonIntent(serde_json::json!({ "n": "seven" })));
        #[derive(serde::Deserialize, Debug)]
        struct Wanted {
            #[allow(dead_code)]
            n: u32,
        }
        let reason = goal.parse::<Wanted>().expect_err("`seven` is not a u32");
        assert!(!reason.may_reach_model(), "{reason:?}");
        assert!(
            matches!(&reason, crate::Reason::Defect { raw: Some(_), .. }),
            "{reason:?}"
        );
    }
}
