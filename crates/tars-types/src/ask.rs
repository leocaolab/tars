//! Defines what a parked task asks, who may answer it, and the reply that answers it.
//!
//! A task parks at one place at a time, so a reply carries no site: it answers the ask the
//! task is parked on. The kind of ask and who may answer it are known when the task parks
//! and are stored with the park, so the durable store refuses a reply that does not answer
//! it ([`fits`]) or comes from someone not asked, before writing anything.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AgentKey, CapName};

/// Who answers an ask: a person, another agent, or a policy that rules automatically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "approver", rename_all = "snake_case")]
pub enum Approver {
    Person { id: String },
    Agent { key: AgentKey },
    Policy { name: String },
}

/// Who may answer one ask: at least one approver, none named twice.
///
/// An ask nobody may answer would park its task until the deadline whatever happens, so
/// the empty set is refused where the set is made, and so is a repeat, which would say the
/// same person twice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Approver>", into = "Vec<Approver>")]
pub struct Approvers(Vec<Approver>);

/// Why a list of approvers is not an [`Approvers`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApproversError {
    #[error("an ask names nobody who may answer it")]
    Empty,
    #[error("an ask names {0:?} twice among who may answer it")]
    Repeated(Approver),
}

impl Approvers {
    /// Admits `approvers` as who may answer an ask.
    ///
    /// # Errors
    ///
    /// [`ApproversError::Empty`] for no approver, [`ApproversError::Repeated`] for one named
    /// twice.
    pub fn new(approvers: Vec<Approver>) -> Result<Self, ApproversError> {
        if approvers.is_empty() {
            return Err(ApproversError::Empty);
        }
        for (i, a) in approvers.iter().enumerate() {
            if approvers[..i].contains(a) {
                return Err(ApproversError::Repeated(a.clone()));
            }
        }
        Ok(Self(approvers))
    }

    /// Returns the set holding only `approver`.
    pub fn one(approver: Approver) -> Self {
        Self(vec![approver])
    }

    /// Returns whether `approver` may answer.
    pub fn contains(&self, approver: &Approver) -> bool {
        self.0.contains(approver)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Approver> {
        self.0.iter()
    }
}

impl TryFrom<Vec<Approver>> for Approvers {
    type Error = ApproversError;

    fn try_from(approvers: Vec<Approver>) -> Result<Self, ApproversError> {
        Self::new(approvers)
    }
}

impl From<Approvers> for Vec<Approver> {
    fn from(approvers: Approvers) -> Self {
        approvers.0
    }
}

/// What a parked task asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ask", rename_all = "snake_case")]
pub enum AgentAsk {
    /// A cap call waiting for approval.
    Approval { cap: CapName, question: String },
    /// A question the body put out.
    Question { question: String },
}

impl AgentAsk {
    /// Returns the words the one answering is shown.
    pub fn question(&self) -> &str {
        match self {
            AgentAsk::Approval { question, .. } | AgentAsk::Question { question } => question,
        }
    }
}

/// One answer to an [`AgentAsk`], and who gave it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum AgentReply {
    /// Lets the cap call waiting for approval run.
    Approve { by: Approver },
    /// Refuses the cap call waiting for approval, in the approver's words.
    Deny { by: Approver, said: String },
    /// Answers a question.
    Answer { by: Approver, value: Value },
}

impl AgentReply {
    /// Returns who gave this reply.
    pub fn by(&self) -> &Approver {
        match self {
            AgentReply::Approve { by }
            | AgentReply::Deny { by, .. }
            | AgentReply::Answer { by, .. } => by,
        }
    }
}

/// Returns whether `reply` answers `ask`: an approval or a refusal answers an approval, and
/// an answer answers a question.
pub fn fits(ask: &AgentAsk, reply: &AgentReply) -> bool {
    match (ask, reply) {
        (AgentAsk::Approval { .. }, AgentReply::Approve { .. } | AgentReply::Deny { .. }) => true,
        (AgentAsk::Question { .. }, AgentReply::Answer { .. }) => true,
        (AgentAsk::Approval { .. }, AgentReply::Answer { .. }) => false,
        (AgentAsk::Question { .. }, AgentReply::Approve { .. } | AgentReply::Deny { .. }) => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn approval() -> AgentAsk {
        AgentAsk::Approval {
            cap: CapName::new("net.post"),
            question: "post?".into(),
        }
    }

    fn question() -> AgentAsk {
        AgentAsk::Question {
            question: "which branch?".into(),
        }
    }

    fn person() -> Approver {
        Approver::Person { id: "ana".into() }
    }

    fn approve() -> AgentReply {
        AgentReply::Approve { by: person() }
    }

    fn deny() -> AgentReply {
        AgentReply::Deny {
            by: person(),
            said: "no".into(),
        }
    }

    fn answer() -> AgentReply {
        AgentReply::Answer {
            by: person(),
            value: json!("main"),
        }
    }

    #[test]
    fn who_may_answer_is_somebody_and_nobody_twice() {
        assert_eq!(Approvers::new(Vec::new()), Err(ApproversError::Empty));
        assert_eq!(
            Approvers::new(vec![person(), person()]),
            Err(ApproversError::Repeated(person()))
        );
        let policy = Approver::Policy {
            name: "auto".into(),
        };
        let two = Approvers::new(vec![person(), policy.clone()]).expect("two distinct approvers");
        assert!(two.contains(&policy) && two.contains(&person()));
    }

    #[test]
    fn a_stored_empty_or_repeated_set_does_not_decode() {
        assert!(serde_json::from_value::<Approvers>(json!([])).is_err());
        let ana = json!({ "approver": "person", "id": "ana" });
        assert!(serde_json::from_value::<Approvers>(json!([ana, ana])).is_err());
        assert_eq!(
            serde_json::from_value::<Approvers>(json!([ana])).expect("one approver"),
            Approvers::one(person())
        );
    }

    #[test]
    fn an_approval_is_answered_by_approving_or_refusing_and_nothing_else() {
        assert!(fits(&approval(), &approve()));
        assert!(fits(&approval(), &deny()));
        assert!(!fits(&approval(), &answer()));
    }

    #[test]
    fn a_question_is_answered_by_an_answer_and_nothing_else() {
        assert!(fits(&question(), &answer()));
        assert!(!fits(&question(), &approve()));
        assert!(!fits(&question(), &deny()));
    }
}
