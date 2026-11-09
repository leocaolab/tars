use crate::{Millis, Usage};

/// The most bytes of raw text a durable record keeps: enough to see what the model said,
/// not so much that a runaway response becomes the trajectory.
pub const RECORD_RAW_MAX: usize = 2_000;

#[derive(Debug)]
pub enum Outcome {
    Success(Refs),
    Failure(Reason),
    Parked(Wait),
}

impl Outcome {
    pub fn success(summary: impl Into<String>) -> Self {
        Self::Success(Refs::new(summary))
    }

    pub fn failure(reason: Reason) -> Self {
        Self::Failure(reason)
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }

    pub fn is_parked(&self) -> bool {
        matches!(self, Self::Parked(_))
    }
}

/// The reportable result and accounting of a successful step; its effects already
/// happened through its caps.
///
/// A task commits this to the durable store, so it deserializes too and a reader of a finished
/// task gets it back typed.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Refs {
    pub summary: String,
    pub data: Option<serde_json::Value>,
    pub usage: Usage,
}

impl Refs {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            data: None,
            usage: Usage::default(),
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }
}

/// What a finaliser did with its resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Release {
    Returned,
    /// The resource is kept in place on purpose, for a human to look at (a worktree holding
    /// uncommitted changes); `what` is the finaliser's own account of what is kept and why.
    /// It is an outcome, not a failure: nothing went wrong, and nothing was lost.
    Retained {
        what: String,
    },
}

/// A finaliser that returns `()` has nothing it could leave behind: it gave the resource back.
impl From<()> for Release {
    fn from((): ()) -> Self {
        Release::Returned
    }
}

/// Why a step could not complete. A step that ran and found its answer was "no" returns a
/// `Success` the caller interprets.
#[derive(Debug, thiserror::Error)]
pub enum Reason {
    #[error("cancelled")]
    Cancelled,
    #[error("bad goal: {0}")]
    BadGoal(String),
    #[error("denied: {0}")]
    Denied(String),
    /// Somebody was asked and did not answer by the deadline the asker set. It differs from
    /// [`Denied`](Self::Denied), which is an answer: nobody said no, and asking again later
    /// may be answered.
    #[error("unanswered: {0}")]
    Unanswered(String),
    /// A deterministic failure not classified further; it is handled like
    /// [`Defect`](Self::Defect). Producers should use [`InvalidInput`](Self::InvalidInput) or
    /// `Defect` instead, which say whether the model may see it.
    #[error("execution failed: {0}")]
    Execution(String),
    /// Text from outside the program was bad: what the model wrote in this run, what an
    /// external tool returned. It goes back into the model's context so the model can correct
    /// itself.
    ///
    /// Who wrote the broken text decides it, not whose fault it was, since where it fails
    /// often cannot tell: arguments the model wrote that do not decode are `InvalidInput` even
    /// when our schema misled it. Something of ours that is broken is a
    /// [`Defect`](Self::Defect).
    ///
    /// `raw` is the text the producer was holding when it failed, which is what the model
    /// needs to fix itself; a producer that shortens it says how much it cut
    /// ([`invalid_input_truncated`](Self::invalid_input_truncated)).
    #[error("invalid input: {detail}")]
    InvalidInput { raw: String, detail: String },
    /// Something of ours is broken: our code, our config, data we stored, a dependency —
    /// serde failing on a type we own, a missing config file, two resources registered under
    /// one name, an unreachable durable store, a corrupt journal. It never reaches the model, which
    /// did not write it, cannot fix it, and would be shown internal paths and connection
    /// strings.
    ///
    /// `raw` is the text the producer was holding when it failed (the stored data that did not
    /// decode, the result that was not the shape it promised), kept for the human who fixes
    /// it; [`defect`](Self::defect) builds one from a producer that holds none.
    #[error("defect: {detail}")]
    Defect { detail: String, raw: Option<String> },
    /// A reason read back from a durable record whose original variant could not be
    /// rebuilt (see [`JournaledReason`]).
    ///
    /// It carries the original's [`error_class`](Self::error_class),
    /// [`handler`](Self::audience), message and `raw` as they were recorded; what is lost
    /// is only the `match` on the original variant.
    #[error("{text}")]
    Replayed {
        class: crate::ErrorClass,
        handler: Audience,
        text: String,
        raw: Option<String>,
    },
    #[error("transient: {0}")]
    Transient(String),
    /// A call the step made (a cap, an LLM) failed, and the callee did not call the failure
    /// transient: a patch that does not apply, a file that is not there, a provider that
    /// refused the request. Another try is not known to pass, and the call may already have
    /// had its effect, so the step is not re-run; the model reads the callee's own words and
    /// can change what it asks for.
    #[error("call failed: {0}")]
    CallFailed(String),
    /// An allowance the caller set ran out (turns, wall clock, tokens) before the work
    /// finished. The work did not fail, and re-running under the same ceiling reproduces
    /// this.
    #[error("ran out of {allowance}: the ceiling was {limit}, and the work was not finished")]
    Exhausted {
        allowance: Allowance,
        limit: u64,
        last_words: Option<String>,
    },
    /// Our code panicked, and the run ended with this instead of the process dying.
    /// `payload` is the panic message clipped to [`RECORD_RAW_MAX`], or `None` when the
    /// panic carried no text.
    #[error("panicked in {site}: {}", panic_words(.payload))]
    Panicked {
        site: &'static str,
        payload: Option<String>,
    },
    /// The loop stopped after `turns` turns in a row in which the model answered and
    /// nothing reached the world.
    ///
    /// `last_words` keeps the model's last answer, so a reader can tell an unresponsive
    /// model from a format the decoder did not handle.
    #[error(
        "stalled: {turns} turn(s) running, the model answered and nothing it said \
             reached the world"
    )]
    Stalled {
        turns: u32,
        last_words: Option<String>,
    },
    /// The task was interrupted `times` times in a row in the same step, and the runner
    /// would not resume it into that step again.
    ///
    /// An interruption is the process dying under the task (a kill, a crash, a lost
    /// host), noticed when its lease is reclaimed. Resuming replays the journal into the
    /// step it died in, so a step that itself kills the process dies on every restart.
    /// `at` is the number of journal entries the task held at every one of those
    /// interruptions: the step that keeps dying is the one that would write the next.
    #[error(
        "interrupted {times} times in a row in the same step (the one after journal \
         entry {at}); resuming into it again would only repeat that"
    )]
    Unresumable { at: u64, times: u32 },
    /// The durable store refused a write from this run because the run does not hold its task: its
    /// lease was reclaimed while it ran, or, run with nothing supervising it, another run
    /// claimed the task. The task is `status` now. Nothing this run does lands any more:
    /// what the task holds and how it ends belong to whoever holds it, if anyone does.
    #[error("the durable store refused this run's write because the run does not hold its task (its lease was reclaimed, or another run claimed it); the task is `{}` now", .status.as_str())]
    LeaseLost { status: crate::Status },
}

fn panic_words(payload: &Option<String>) -> &str {
    match payload {
        Some(p) => p.as_str(),
        None => "the panic payload was neither a String nor a &str, so it carried no message",
    }
}

/// Which ceiling ran out; it is serialized in the durable record ([`ExactReason`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Allowance {
    Turns,
    /// The limit is in milliseconds.
    Wallclock,
    Tokens,
    /// The rounds a reconcile loop may take toward a fixpoint.
    Rounds,
}

impl std::fmt::Display for Allowance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Allowance::Turns => "turns",
            Allowance::Wallclock => "wall-clock time",
            Allowance::Tokens => "tokens",
            Allowance::Rounds => "rounds",
        })
    }
}

impl Reason {
    pub fn classification(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::BadGoal(_) => "bad_goal",
            Self::Denied(_) => "denied",
            Self::Unanswered(_) => "unanswered",
            Self::Execution(_) => "execution",
            Self::InvalidInput { .. } => "invalid_input",
            Self::Defect { .. } => "defect",
            Self::Replayed { .. } => "replayed",
            Self::Transient(_) => "transient",
            Self::CallFailed(_) => "call_failed",
            Self::Exhausted { .. } => "exhausted",
            Self::Panicked { .. } => "panicked",
            Self::Stalled { .. } => "stalled",
            Self::Unresumable { .. } => "unresumable",
            Self::LeaseLost { .. } => "lease_lost",
        }
    }

    /// Returns whether the cause goes away with time: `Transient` is transient, a replayed
    /// reason keeps the class it was recorded with, and every other variant is `Permanent`.
    pub fn error_class(&self) -> crate::ErrorClass {
        match self {
            Self::Replayed { class, .. } => *class,
            Self::Transient(_) => crate::ErrorClass::Transient,
            // Re-running under the same ceiling hits the same ceiling; what has to change
            // is the caller's configuration.
            Self::Exhausted { .. } => crate::ErrorClass::Permanent,
            // A bug in our code runs the same way twice, until someone changes the code.
            Self::Panicked { .. } => crate::ErrorClass::Permanent,
            // The same protocol against the same model stalls in the same place; what has
            // to change is the decoder or the prompt.
            Self::Stalled { .. } => crate::ErrorClass::Permanent,
            // Resuming is the thing that keeps failing; another resume is the same act.
            Self::Unresumable { .. } => crate::ErrorClass::Permanent,
            // This run is no longer the task's; running it again would not make it so.
            Self::LeaseLost { .. } => crate::ErrorClass::Permanent,
            Self::Cancelled
            | Self::BadGoal(_)
            | Self::Denied(_)
            | Self::Unanswered(_)
            | Self::Execution(_)
            | Self::InvalidInput { .. }
            | Self::CallFailed(_)
            | Self::Defect { .. } => crate::ErrorClass::Permanent,
        }
    }

    /// Returns who should see this failure: the agent, which folds it back into the
    /// model's context, or the runtime, which ends the step and never prompts with it.
    ///
    /// It is independent of [`error_class`](Self::error_class): [`InvalidInput`](Self::InvalidInput)
    /// and [`Defect`](Self::Defect) are both `Permanent` and have opposite audiences.
    pub fn audience(&self) -> Audience {
        match self {
            // Someone above asked to stop; a supervisor must not charge a cancellation it
            // issued itself to a child's retry budget.
            Self::Cancelled => Audience::Interrupt,
            // A bad model output, a refused or failed tool, a rate limit: the model can
            // reason about each and route around it.
            Self::InvalidInput { .. }
            | Self::BadGoal(_)
            | Self::Denied(_)
            | Self::Unanswered(_)
            | Self::Transient(_)
            | Self::CallFailed(_) => Audience::Agent,
            // A model cannot raise its own ceiling, and the run is over when this is
            // produced, so there is no next turn to fold it into.
            Self::Exhausted { .. } => Audience::Runtime,
            // A model cannot fix our panic, and showing it one leaks our internals to the
            // provider.
            Self::Panicked { .. } => Audience::Runtime,
            // Whether the model went quiet or our decoder could not read it, the run is
            // over, and what has to change (a decoder, a prompt) is on our side.
            Self::Stalled { .. } => Audience::Runtime,
            // The step died with the process, so no model turn saw it happen and none
            // is left to show it to.
            Self::Unresumable { .. } => Audience::Runtime,
            // The run is no longer the task's, so no turn of it is left to show this to.
            Self::LeaseLost { .. } => Audience::Runtime,
            // `Execution` is unclassified, and showing the model an internal bug is worse
            // than withholding a message it could not have used.
            Self::Defect { .. } | Self::Execution(_) => Audience::Runtime,
            Self::Replayed { handler, .. } => *handler,
        }
    }

    /// Returns whether re-running could plausibly succeed: `Transient`, and replayed reasons
    /// recorded as transient.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transient(_) => true,
            Self::Replayed { class, .. } => *class == crate::ErrorClass::Transient,
            Self::Cancelled
            | Self::BadGoal(_)
            | Self::Denied(_)
            | Self::Unanswered(_)
            | Self::Execution(_)
            | Self::InvalidInput { .. }
            | Self::CallFailed(_)
            | Self::Defect { .. }
            // The same ceiling produces the same ending, after spending the allowance again.
            | Self::Exhausted { .. }
            // A panic re-panics, and a decoder that could not read the model's answers
            // cannot read them the second time either.
            | Self::Panicked { .. }
            | Self::Stalled { .. }
            // A retry IS a resume into the step that keeps killing the process.
            | Self::Unresumable { .. }
            | Self::LeaseLost { .. } => false,
        }
    }
}

/// Who should see a [`Reason`] ([`Reason::audience`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    /// The model sees it as an observation and adapts (fixes its JSON, picks another
    /// tool).
    Agent,
    /// The runtime's: it never renders into a prompt, and the step ends.
    Runtime,
    /// An interruption from above, not a failure; it is not charged to a child's failure
    /// budget.
    Interrupt,
}

impl Reason {
    /// Returns this reason as written into the durable record: its message followed by its
    /// raw text, clipped to [`RECORD_RAW_MAX`] bytes on a char boundary with the cut
    /// stated.
    ///
    /// `Display` leaves the raw out, because a whole model response has no place in every
    /// log line; the record is the only copy that outlives the process, so it keeps it.
    pub fn record(&self) -> String {
        // Rendered by the durable form, which durable store rows and fiber journals store, so this
        // matches what a reader of the record gets back.
        JournaledReason::from(self).record()
    }

    pub(crate) fn clip(raw: &str, max: usize) -> String {
        if raw.len() <= max {
            return raw.to_string();
        }
        let mut end = max;
        while end > 0 && !raw.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}… [{} more bytes elided]", &raw[..end], raw.len() - end)
    }

    /// Returns a [`Defect`](Self::Defect) with no raw text.
    pub fn defect(detail: impl Into<String>) -> Self {
        Self::Defect {
            detail: detail.into(),
            raw: None,
        }
    }

    /// Returns an [`InvalidInput`](Self::InvalidInput) whose `raw` is cut to `max` bytes on a char
    /// boundary, with the number of bytes cut stated in the text.
    pub fn invalid_input_truncated(raw: &str, detail: impl Into<String>, max: usize) -> Self {
        Self::InvalidInput {
            raw: Self::clip(raw, max),
            detail: detail.into(),
        }
    }

    /// Returns a [`Panicked`](Self::Panicked) from the payload `catch_unwind` hands back.
    ///
    /// A `&'static str` or `String` payload (what `panic!` produces) is the message,
    /// clipped to [`RECORD_RAW_MAX`]; any other payload carried no text and gives `None`.
    /// `site` names the catch (`"World::dispatch"`, not a sentence), so failures can be
    /// counted by site with `grep`.
    pub fn panicked(site: &'static str, payload: &(dyn std::any::Any + Send)) -> Self {
        let message = payload
            .downcast_ref::<&'static str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned());
        Self::Panicked {
            site,
            payload: message.map(|m| Self::clip(&m, RECORD_RAW_MAX)),
        }
    }

    pub fn exhausted(allowance: Allowance, limit: u64, last_words: Option<&str>) -> Self {
        Self::Exhausted {
            allowance,
            limit,
            last_words: last_words.map(|w| Self::clip(w, RECORD_RAW_MAX)),
        }
    }

    /// Returns a [`Stalled`](Self::Stalled) carrying the model's last answer, clipped.
    ///
    /// `last_words` is `None` when the loop never got an answer to keep: a stall made
    /// entirely of turns the loop refused to send.
    pub fn stalled(turns: u32, last_words: Option<&str>) -> Self {
        Self::Stalled {
            turns,
            last_words: last_words.map(|w| Self::clip(w, RECORD_RAW_MAX)),
        }
    }

    /// Returns this reason as it comes back from a durable record: itself for the
    /// variants [`ExactReason`] holds, otherwise [`Replayed`](Self::Replayed) with the
    /// same class and handler.
    pub fn to_replayable(&self) -> Self {
        JournaledReason::from(self).into()
    }

    /// Returns whether this reason may be rendered into an LLM prompt, which is when its
    /// handler is [`Audience::Agent`].
    pub fn may_reach_model(&self) -> bool {
        self.audience() == Audience::Agent
    }
}

/// A [`Reason`] as a durable store row or a fiber journal stores it.
///
/// Not every variant survives serde, so this stores the [`error_class`](Reason::error_class) and
/// [`handler`](Reason::audience) consumers branch on, the message and the raw text. It
/// reads back as the original variant when [`exact`](Self::exact) holds one, otherwise
/// as [`Reason::Replayed`], which says it is a reconstruction rather than guess a
/// variant.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JournaledReason {
    pub class: crate::ErrorClass,
    pub handler: Audience,
    /// The reason's [`Display`](std::fmt::Display) text.
    pub text: String,
    /// The text the producer was holding when it failed, when it had any; kept apart from
    /// `text` so a reader that wants a sentence is not handed a whole model response.
    pub raw: Option<String>,
    /// The original variant, when it is one [`ExactReason`] holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<ExactReason>,
}

/// The reasons that come back from a durable record as themselves.
///
/// These carry numbers a caller branches on (which ceiling, how many turns, which step
/// and how many interruptions) that [`Reason::Replayed`]'s text cannot give back; a
/// variant whose payload is a string loses nothing to `Replayed`. A caller that tells
/// "ran out of the turns you gave me" from "broke" by matching [`Reason::Exhausted`]
/// keeps that match across a journal.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExactReason {
    Exhausted {
        allowance: Allowance,
        limit: u64,
        last_words: Option<String>,
    },
    Stalled {
        turns: u32,
        last_words: Option<String>,
    },
    Unresumable {
        at: u64,
        times: u32,
    },
    /// A body branches on a refusal and on a lapse, so both come back as themselves.
    Denied {
        why: String,
    },
    Unanswered {
        why: String,
    },
}

impl JournaledReason {
    /// Returns the record of a task a supervisor cancelled, with [`Reason::Cancelled`]'s
    /// class and handler and the canceller's own words ("its parent failed").
    pub fn cancelled(text: impl Into<String>) -> Self {
        Self::from(&Reason::Cancelled).with_text(text)
    }

    fn with_text(self, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..self
        }
    }

    /// Returns the text with the raw appended when there is one, clipped to
    /// [`RECORD_RAW_MAX`]; [`Reason::record`] returns the same string.
    pub fn record(&self) -> String {
        match &self.raw {
            Some(raw) => format!("{} — raw: {}", self.text, Reason::clip(raw, RECORD_RAW_MAX)),
            None => self.text.clone(),
        }
    }
}

impl std::fmt::Display for JournaledReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl From<&Reason> for JournaledReason {
    fn from(r: &Reason) -> Self {
        JournaledReason {
            class: r.error_class(),
            handler: r.audience(),
            text: r.to_string(),
            exact: match r {
                Reason::Exhausted {
                    allowance,
                    limit,
                    last_words,
                } => Some(ExactReason::Exhausted {
                    allowance: *allowance,
                    limit: *limit,
                    last_words: last_words.clone(),
                }),
                Reason::Stalled { turns, last_words } => Some(ExactReason::Stalled {
                    turns: *turns,
                    last_words: last_words.clone(),
                }),
                Reason::Unresumable { at, times } => Some(ExactReason::Unresumable {
                    at: *at,
                    times: *times,
                }),
                Reason::Denied(why) => Some(ExactReason::Denied { why: why.clone() }),
                Reason::Unanswered(why) => Some(ExactReason::Unanswered { why: why.clone() }),
                _ => None,
            },
            raw: match r {
                Reason::InvalidInput { raw, .. } => Some(raw.clone()),
                Reason::Defect { raw, .. } => raw.clone(),
                Reason::Replayed { raw, .. } => raw.clone(),
                // The model's last answer is what makes a stall or a spent allowance
                // diagnosable.
                Reason::Stalled { last_words, .. } | Reason::Exhausted { last_words, .. } => {
                    last_words.clone()
                }
                // A panic's message is already in `text` (see the variant's `#[error]`
                // string). Putting it here as well would print it twice in `record`.
                _ => None,
            },
        }
    }
}

impl From<JournaledReason> for Reason {
    fn from(j: JournaledReason) -> Self {
        match j.exact {
            Some(ExactReason::Exhausted {
                allowance,
                limit,
                last_words,
            }) => Reason::Exhausted {
                allowance,
                limit,
                last_words,
            },
            Some(ExactReason::Stalled { turns, last_words }) => {
                Reason::Stalled { turns, last_words }
            }
            Some(ExactReason::Unresumable { at, times }) => Reason::Unresumable { at, times },
            Some(ExactReason::Denied { why }) => Reason::Denied(why),
            Some(ExactReason::Unanswered { why }) => Reason::Unanswered(why),
            None => Reason::Replayed {
                class: j.class,
                handler: j.handler,
                text: j.text,
                raw: j.raw,
            },
        }
    }
}

/// What a parked step waits for.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "wait", rename_all = "snake_case")]
pub enum Wait {
    /// Its own children: one of them waking it is certain, so it needs no deadline.
    Children,
    /// An answer to `ask` from one of `from`. The one asked may never answer, so it wakes by
    /// `until` (epoch millis) whether or not anyone did.
    Answer {
        ask: crate::AgentAsk,
        from: crate::Approvers,
        until: Millis,
    },
}

impl Wait {
    /// Returns when the step wakes unanswered, or `None` while only children are awaited.
    pub fn until(&self) -> Option<Millis> {
        match self {
            Wait::Children => None,
            Wait::Answer { until, .. } => Some(*until),
        }
    }
}

/// Returns whether a deadline `until` has passed at `now`, both epoch millis.
pub fn expired(now: Millis, until: Millis) -> bool {
    now >= until
}

#[cfg(test)]
mod tests {
    use super::Reason;

    use super::{Audience, JournaledReason};

    #[test]
    fn invalid_and_defect_agree_on_retry_and_differ_on_owner() {
        let invalid = Reason::InvalidInput {
            raw: "{oops".into(),
            detail: "bad json".into(),
        };
        let defect = Reason::defect("durable store unreachable");

        assert!(!invalid.is_retryable());
        assert!(!defect.is_retryable());
        assert_eq!(
            invalid.error_class(),
            defect.error_class(),
            "same retry axis"
        );

        assert_eq!(
            invalid.audience(),
            Audience::Agent,
            "the model must SEE its bad output"
        );
        assert_eq!(
            defect.audience(),
            Audience::Runtime,
            "the model must NEVER see our bug"
        );
    }

    #[test]
    fn a_failed_call_reaches_the_model_and_is_not_rerun() {
        let failed = Reason::CallFailed("patch does not apply".into());
        assert_eq!(failed.audience(), Audience::Agent);
        assert!(!failed.is_retryable());
        assert_eq!(failed.error_class(), crate::ErrorClass::Permanent);

        let replayed = Reason::from(JournaledReason::from(&failed));
        assert_eq!(replayed.audience(), Audience::Agent);
        assert!(!replayed.is_retryable());
        assert!(replayed.to_string().contains("patch does not apply"));
    }

    #[test]
    fn a_lost_lease_is_the_runtimes_and_is_not_rerun() {
        let lost = Reason::LeaseLost {
            status: crate::Status::Running,
        };
        assert_eq!(lost.audience(), Audience::Runtime);
        assert!(!lost.is_retryable());
        assert!(!lost.may_reach_model());
        assert!(lost.to_string().contains("`running`"), "{lost}");
    }

    #[test]
    fn cancelled_is_an_interrupt_not_an_agent_error() {
        assert_eq!(Reason::Cancelled.audience(), Audience::Interrupt);
    }

    #[test]
    fn only_agent_owned_reasons_may_reach_the_model() {
        assert!(
            Reason::InvalidInput {
                raw: "x".into(),
                detail: "d".into()
            }
            .may_reach_model()
        );
        assert!(Reason::Transient("429".into()).may_reach_model());
        assert!(!Reason::defect("serde on our own type").may_reach_model());
        assert!(!Reason::Execution("legacy bucket".into()).may_reach_model());
        assert!(!Reason::Cancelled.may_reach_model());
    }

    #[test]
    fn invalid_truncated_keeps_a_prefix_and_says_what_it_cut() {
        let raw = "a".repeat(100);
        match Reason::invalid_input_truncated(&raw, "too long", 10) {
            Reason::InvalidInput { raw, detail } => {
                assert!(
                    raw.starts_with("aaaaaaaaaa"),
                    "keeps the real prefix: {raw}"
                );
                assert!(
                    raw.contains("90 more bytes elided"),
                    "states the cut: {raw}"
                );
                assert_eq!(detail, "too long");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn invalid_truncated_cuts_on_a_char_boundary() {
        let raw = "中文中文中文";
        match Reason::invalid_input_truncated(raw, "d", 7) {
            Reason::InvalidInput { raw, .. } => assert!(raw.starts_with("中文"), "got {raw}"),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn a_caught_panic_is_ours_and_never_reaches_the_model() {
        let boom = Reason::panicked("World::dispatch", &"index out of bounds");
        assert_eq!(boom.classification(), "panicked");
        assert_eq!(boom.audience(), Audience::Runtime);
        assert!(!boom.may_reach_model());
        assert!(!boom.is_retryable());
        let said = boom.to_string();
        assert!(
            said.contains("World::dispatch"),
            "the site is named: {said}"
        );
        assert!(
            said.contains("index out of bounds"),
            "and the panic's own words: {said}"
        );
    }

    #[test]
    fn a_panic_with_no_message_says_so_in_words() {
        #[derive(Debug)]
        struct Odd;
        let boom = Reason::panicked("World::view", &Odd);
        let said = boom.to_string();
        assert!(said.contains("neither a String nor a &str"), "{said}");
        assert!(
            !said.contains("unknown"),
            "no sterile token stands in for it: {said}"
        );
    }

    #[test]
    fn a_runaway_panic_message_is_clipped_and_admits_it() {
        let huge = "x".repeat(super::RECORD_RAW_MAX * 2);
        match Reason::panicked("World::observe", &huge) {
            Reason::Panicked {
                payload: Some(p), ..
            } => {
                assert!(p.contains("more bytes elided"), "states the cut");
                assert!(p.len() < huge.len());
            }
            other => panic!("expected a payload, got {other:?}"),
        }
    }

    #[test]
    fn a_stall_is_told_apart_from_a_spent_budget() {
        let stalled = Reason::stalled(6, Some("<｜｜DSML｜｜tool_calls>"));
        let spent = Reason::exhausted(super::Allowance::Turns, 60, None);
        assert_ne!(stalled.classification(), spent.classification());
        assert_eq!(stalled.classification(), "stalled");
        assert_eq!(
            stalled.audience(),
            Audience::Runtime,
            "the run is over; nothing to prompt"
        );
        assert!(!stalled.is_retryable());
        assert!(
            stalled.to_string().contains('6'),
            "the count travels: {stalled}"
        );
    }

    /// A stall's last answer rides in the record's `raw`, so the one-string rendering
    /// shows it.
    #[test]
    fn a_stalls_last_words_survive_the_journal() {
        let stalled = Reason::stalled(6, Some("<｜｜DSML｜｜invoke name=\"read\">"));
        let journaled = JournaledReason::from(&stalled);
        assert_eq!(journaled.handler, Audience::Runtime);
        assert!(
            journaled
                .raw
                .as_deref()
                .unwrap_or_default()
                .contains("DSML"),
            "the model's own markup came through: {:?}",
            journaled.raw
        );
        assert!(
            stalled.record().contains("DSML"),
            "and the one-string rendering has it too"
        );
    }

    #[test]
    fn a_stall_with_no_answer_kept_carries_no_raw() {
        assert_eq!(JournaledReason::from(&Reason::stalled(6, None)).raw, None);
    }

    #[test]
    fn only_transient_and_retriable_providers_retry() {
        assert!(Reason::Transient("worker crashed".into()).is_retryable());
        assert!(!Reason::Execution("decode bug".into()).is_retryable());
        assert!(!Reason::BadGoal("malformed".into()).is_retryable());
        assert!(!Reason::Denied("cap".into()).is_retryable());
        assert!(!Reason::Cancelled.is_retryable());
    }
}
