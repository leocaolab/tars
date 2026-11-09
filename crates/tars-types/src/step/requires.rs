//! Declares what a [`Step`](crate::Step) needs from the world before it can run, as data
//! the runtime checks at claim time.
//!
//! The requirement is data rather than a type parameter (`Step<R>`) because `Step` must
//! stay object-safe for the registry. A step declares its needs, and the runtime checks
//! the grant against them before `exec` (see [`StepContext::provide`](crate::StepContext::provide)):
//! a missing cap is caught before the first token is spent, and the declarations alone
//! answer which steps of a job can reach the network.
//!
//! The durable store (`FiberStore`, in `tars-storage`) is not
//! declared here: it is the engine's own, held at construction, so a step cannot opt
//! out of journaling.
use crate::CapName;
use crate::cap::Effect;

/// One cap a step needs, and the blast-radius tier it intends to use it at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapReq {
    /// The cap's name, as in [`CapSpec::name`](crate::cap::CapSpec::name).
    pub name: CapName,
    /// The tier this step will use the cap at; a grant below it does not satisfy the
    /// requirement.
    pub effect: Effect,
    /// When set, a missing grant lets the step run without the cap.
    pub optional: bool,
}

impl CapReq {
    pub fn new(name: CapName, effect: Effect) -> Self {
        Self {
            name,
            effect,
            optional: false,
        }
    }

    /// Marks the requirement optional: a missing grant no longer blocks the run.
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }
}

/// Every cap a step needs; the empty value needs nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requires {
    pub caps: Vec<CapReq>,
}

impl Requires {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn of(caps: impl IntoIterator<Item = (&'static str, Effect)>) -> Self {
        Self {
            caps: caps
                .into_iter()
                .map(|(n, e)| CapReq::new(CapName::new(n), e))
                .collect(),
        }
    }

    pub fn and(mut self, req: CapReq) -> Self {
        self.caps.push(req);
        self
    }

    /// Returns the highest tier this step asks for, or `None` when it needs no caps.
    pub fn max_effect(&self) -> Option<Effect> {
        self.caps.iter().map(|c| c.effect).max()
    }

    pub fn wants_egress(&self) -> bool {
        self.max_effect() == Some(Effect::External)
    }
}

/// Why a grant did not satisfy a [`Requires`].
///
/// It carries what was granted beside the gaps, so a reader can tell a misnamed cap from
/// one that was never wired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unmet {
    pub missing: Vec<CapReq>,
    /// Granted, but at a lower tier than the step declared it would use.
    pub under_granted: Vec<(CapReq, Effect)>,
    /// The names of every granted cap, sorted.
    pub granted: Vec<CapName>,
}

impl std::fmt::Display for Unmet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.missing.is_empty() {
            let list: Vec<String> = self
                .missing
                .iter()
                .map(|c| format!("{}({:?})", c.name, c.effect))
                .collect();
            write!(f, "missing cap(s): {}", list.join(", "))?;
        }
        if !self.under_granted.is_empty() {
            if !self.missing.is_empty() {
                write!(f, "; ")?;
            }
            let list: Vec<String> = self
                .under_granted
                .iter()
                .map(|(c, got)| format!("{} needs {:?} but grant has {:?}", c.name, c.effect, got))
                .collect();
            write!(f, "under-granted: {}", list.join(", "))?;
        }
        write!(
            f,
            "; granted: [{}]",
            self.granted
                .iter()
                .map(CapName::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl std::error::Error for Unmet {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_requires_needs_nothing_and_wants_no_egress() {
        let r = Requires::none();
        assert!(r.caps.is_empty());
        assert_eq!(r.max_effect(), None);
        assert!(!r.wants_egress());
    }

    #[test]
    fn max_effect_surfaces_egress_without_running_anything() {
        let quiet = Requires::of([
            ("fs.read", Effect::Read),
            ("fs.write_file", Effect::WriteLocal),
        ]);
        assert_eq!(quiet.max_effect(), Some(Effect::WriteLocal));
        assert!(!quiet.wants_egress());

        let loud = quiet
            .clone()
            .and(CapReq::new(CapName::new("bash.run"), Effect::External));
        assert!(
            loud.wants_egress(),
            "one External requirement makes the whole step egress-capable"
        );
    }

    #[test]
    fn unmet_display_reports_the_gap_and_the_grant() {
        let unmet = Unmet {
            missing: vec![CapReq::new(
                CapName::new("github.post_comment"),
                Effect::External,
            )],
            under_granted: vec![(
                CapReq::new(CapName::new("fs.write_file"), Effect::WriteLocal),
                Effect::Read,
            )],
            granted: vec![CapName::new("fs.read"), CapName::new("git.diff")],
        };
        let msg = unmet.to_string();
        assert!(msg.contains("github.post_comment"), "{msg}");
        assert!(
            msg.contains("fs.write_file needs WriteLocal but grant has Read"),
            "{msg}"
        );
        assert!(
            msg.contains("granted: [fs.read, git.diff]"),
            "the other half of the truth: {msg}"
        );
    }
}
