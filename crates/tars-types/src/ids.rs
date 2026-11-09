//! Gives every identifier its own type, so a `TenantId` cannot be passed where a `SessionId` is
//! expected. A string id is never empty; a [`CallId`] or [`SessionId`] is a UUIDv7 whose UTC
//! day is read off the id.

use chrono::{DateTime, Days, NaiveDate};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::{NoContext, Timestamp, Uuid};

use crate::Millis;

macro_rules! string_id {
    ($name:ident, $error:ident, $doc:literal) => {
        #[doc = $doc]
        // `Deserialize` is written by hand so the wire refuses an empty id as `parse` does; serde
        // does not accept `transparent` and `try_from` on one derive.
        #[derive(Clone, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        #[doc = concat!("The text was empty, so it is not a [`", stringify!($name), "`].")]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $error;

        impl fmt::Display for $error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), " cannot be empty"))
            }
        }

        impl std::error::Error for $error {}

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Self::admit(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }

        impl TryFrom<String> for $name {
            type Error = $error;

            fn try_from(text: String) -> Result<Self, $error> {
                Self::admit(text)
            }
        }

        impl std::str::FromStr for $name {
            type Err = $error;

            fn from_str(text: &str) -> Result<Self, $error> {
                Self::admit(text.to_owned())
            }
        }

        impl $name {
            // An empty id would otherwise flow silently into IAM scope checks and lookups.
            fn admit(text: String) -> Result<Self, $error> {
                if text.is_empty() {
                    return Err($error);
                }
                Ok(Self(text))
            }

            /// Wraps a literal written in the program. Text from outside the program, or built
            /// at run time, goes through `parse`.
            ///
            /// # Panics
            ///
            /// Panics when `literal` is empty; only a literal reaches here, so the panic is a
            /// typo in the source.
            pub fn new(literal: &'static str) -> Self {
                match Self::admit(literal.to_owned()) {
                    Ok(id) => id,
                    Err(e) => panic!("{e}"),
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    TenantId,
    ParseTenantIdError,
    "Tenant identifier — the hard isolation boundary."
);
string_id!(
    DurableStoreId,
    ParseDurableStoreIdError,
    "Which durable store a record came from — generated when a runner first opens the durable store, and never changed after."
);
string_id!(
    TraceId,
    ParseTraceIdError,
    "Distributed trace identifier; propagated through all layers."
);
string_id!(
    TrajectoryId,
    ParseTrajectoryIdError,
    "Trajectory node identifier."
);
string_id!(
    PrincipalId,
    ParsePrincipalIdError,
    "Principal (caller identity) identifier."
);
string_id!(
    ProviderId,
    ParseProviderIdError,
    "Provider instance identifier (e.g. `openai_main`, `local_qwen`)."
);
string_id!(
    L3HandleId,
    ParseL3HandleIdError,
    "Internal handle for an L3 explicit cache."
);
string_id!(AgentId, ParseAgentIdError, "Agent instance identifier.");
string_id!(
    BatchJobId,
    ParseBatchJobIdError,
    "Batch job identifier returned by the provider on submit."
);
string_id!(
    TaskId,
    ParseTaskIdError,
    "A task's stable identity. It is also the memo key: a task's result is stored under it, so a task with a result is checkpointed and never runs again."
);
string_id!(
    JobId,
    ParseJobIdError,
    "The job (one submission) a task belongs to; an agent's `spawn` cap puts the children it creates into the same job."
);
string_id!(
    AgentKey,
    ParseAgentKeyError,
    "The name an agent is registered under, which a task names to say what runs it."
);
string_id!(
    CapName,
    ParseCapNameError,
    "The name a capability is granted and called by."
);
string_id!(
    BatchItemId,
    ParseBatchItemIdError,
    "Per-item identifier inside a batch — caller-chosen, echoed back in results so each output can be matched to its input. Maps to Anthropic's `custom_id` and OpenAI's `custom_id` on batch lines."
);

impl From<Uuid> for TraceId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid.to_string())
    }
}

impl From<Uuid> for DurableStoreId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid.to_string())
    }
}

/// Exclusive upper bound of a UUIDv7 timestamp: 48 bits of Unix milliseconds.
const V7_MILLIS_END: u64 = 1 << 48;
const MILLIS_PER_SECOND: u64 = 1_000;
const NANOS_PER_MILLI: u64 = 1_000_000;
const MILLIS_PER_DAY: u64 = 86_400_000;

/// A UTC calendar day — the partition day of a call's log rows.
///
/// Every constructor computes it in UTC: [`CallId::day`], or [`UtcDay::of`]
/// for an instant the shell read from its clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcDay(NaiveDate);

impl UtcDay {
    /// Returns the UTC day `at` (epoch ms) falls on; `None` outside chrono's range.
    pub fn of(at: Millis) -> Option<Self> {
        DateTime::from_timestamp_millis(at).map(|t| Self(t.date_naive()))
    }
}

impl fmt::Display for UtcDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.format("%Y-%m-%d"))
    }
}

/// One LLM call's identity: a UUIDv7, so its UTC day is readable from the id
/// itself and a lookup by id knows which day's partition to open.
///
/// Only v7 ids exist — a v4 (or any other version) never becomes a `CallId`, in JSON neither:
/// it is written as the hyphenated UUID and read back through [`TryFrom<Uuid>`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "Uuid", into = "Uuid")]
pub struct CallId(Uuid);

/// Why a value cannot be a [`CallId`].
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallIdError {
    #[error("call start {0} ms is outside a UUIDv7 timestamp (0 ..< 2^48 ms since the Unix epoch)")]
    TimeOutOfRange(Millis),
    #[error("{uuid} is UUID version {version}, a call id must be version 7")]
    NotV7 { uuid: Uuid, version: usize },
}

impl CallId {
    /// Mints the id of a call that starts at `at` (epoch ms, read by the shell).
    ///
    /// # Errors
    ///
    /// Returns `TimeOutOfRange` when `at` is outside `0 ..< 2^48` ms.
    pub fn new_v7(at: Millis) -> Result<Self, CallIdError> {
        mint_v7(at).map(Self).ok_or(CallIdError::TimeOutOfRange(at))
    }

    /// Returns the UTC day this call started on.
    pub fn day(&self) -> UtcDay {
        v7_day(&self.0)
    }
}

/// Returns `at` as a v7 timestamp; `None` outside `0 ..< 2^48` ms.
fn v7_millis(at: Millis) -> Option<u64> {
    u64::try_from(at).ok().filter(|ms| *ms < V7_MILLIS_END)
}

/// A UUIDv7 stamped with `at`; `None` when `at` is outside a v7 timestamp.
fn mint_v7(at: Millis) -> Option<Uuid> {
    let millis = v7_millis(at)?;
    let subsec_nanos = (millis % MILLIS_PER_SECOND) * NANOS_PER_MILLI;
    let ts = Timestamp::from_unix(
        NoContext,
        millis / MILLIS_PER_SECOND,
        // < 1e9, always fits.
        subsec_nanos as u32,
    );
    Some(Uuid::new_v7(ts))
}

/// Returns the UTC day of a v7 id's timestamp: its first 48 bits, big-endian Unix milliseconds
/// (RFC 9562 §5.7).
fn v7_day(uuid: &Uuid) -> UtcDay {
    let b = uuid.as_bytes();
    let millis = u64::from_be_bytes([0, 0, b[0], b[1], b[2], b[3], b[4], b[5]]);
    // A 48-bit timestamp ends in year 10889, far inside chrono's range,
    // so this addition cannot overflow.
    UtcDay(DateTime::UNIX_EPOCH.date_naive() + Days::new(millis / MILLIS_PER_DAY))
}

impl TryFrom<Uuid> for CallId {
    type Error = CallIdError;

    fn try_from(uuid: Uuid) -> Result<Self, Self::Error> {
        match uuid.get_version_num() {
            7 => Ok(Self(uuid)),
            version => Err(CallIdError::NotV7 { uuid, version }),
        }
    }
}

impl From<CallId> for Uuid {
    fn from(id: CallId) -> Self {
        id.0
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CallId({})", self.0)
    }
}

/// A conversation's identity: a UUIDv7 minted by whoever opens the conversation (a
/// `tars_agent::Session`), so ids never collide across processes and its transcript's day is
/// read off the id. A caller outside a conversation names none; there is no free-form session
/// label.
///
/// It is written as the hyphenated UUID and read back only when that UUID is version 7.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(Uuid);

/// Why a value cannot be a [`SessionId`].
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionIdError {
    #[error("SessionId cannot be empty")]
    Empty,
    #[error(
        "session start {0} ms is outside a UUIDv7 timestamp (0 ..< 2^48 ms since the Unix epoch)"
    )]
    TimeOutOfRange(Millis),
    #[error("{uuid} is UUID version {version}, a session id must be version 7")]
    NotV7 { uuid: Uuid, version: usize },
    #[error("{text:?} is not a UUID")]
    NotUuid { text: String, source: uuid::Error },
}

impl SessionId {
    /// Mints the id of a session that opens at `at` (epoch ms, read by the shell).
    ///
    /// # Errors
    ///
    /// Returns `TimeOutOfRange` when `at` is outside `0 ..< 2^48` ms.
    pub fn new_v7(at: Millis) -> Result<Self, SessionIdError> {
        mint_v7(at)
            .map(Self)
            .ok_or(SessionIdError::TimeOutOfRange(at))
    }

    /// Returns the UTC day this session opened on.
    pub fn day(&self) -> UtcDay {
        v7_day(&self.0)
    }
}

impl TryFrom<Uuid> for SessionId {
    type Error = SessionIdError;

    fn try_from(uuid: Uuid) -> Result<Self, Self::Error> {
        match uuid.get_version_num() {
            7 => Ok(Self(uuid)),
            version => Err(SessionIdError::NotV7 { uuid, version }),
        }
    }
}

impl std::str::FromStr for SessionId {
    type Err = SessionIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() {
            return Err(SessionIdError::Empty);
        }
        let uuid = Uuid::parse_str(text).map_err(|source| SessionIdError::NotUuid {
            text: text.to_owned(),
            source,
        })?;
        Self::try_from(uuid)
    }
}

impl TryFrom<String> for SessionId {
    type Error = SessionIdError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<SessionId> for String {
    fn from(id: SessionId) -> Self {
        id.to_string()
    }
}

impl From<SessionId> for TraceId {
    fn from(id: SessionId) -> Self {
        Self(id.to_string())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_distinct_types() {
        let t = TenantId::new("acme");
        let p = ProviderId::new("acme");
        // `assert_eq!(t, p)` does not compile; only their text compares.
        assert_eq!(t.as_str(), p.as_str());
    }

    #[test]
    fn ids_round_trip_through_serde() {
        let t = TenantId::new("acme");
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "\"acme\"");
        let back: TenantId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn a_task_job_agent_or_cap_name_is_refused_when_empty_and_kept_otherwise() {
        assert_eq!(TaskId::try_from(String::new()), Err(ParseTaskIdError));
        assert_eq!(JobId::try_from(String::new()), Err(ParseJobIdError));
        assert_eq!(AgentKey::try_from(String::new()), Err(ParseAgentKeyError));
        assert_eq!(CapName::try_from(String::new()), Err(ParseCapNameError));
        assert_eq!(TaskId::try_from("t".to_owned()).unwrap().as_str(), "t");
        assert_eq!(JobId::try_from("j".to_owned()).unwrap().as_str(), "j");
        assert_eq!(AgentKey::try_from("a".to_owned()).unwrap().as_str(), "a");
        assert_eq!(CapName::try_from("c".to_owned()).unwrap().as_str(), "c");
    }

    #[test]
    fn an_empty_task_id_on_the_wire_is_refused() {
        assert!(serde_json::from_str::<TaskId>("\"\"").is_err());
        assert_eq!(
            serde_json::from_str::<TaskId>("\"t\"").unwrap(),
            TaskId::new("t")
        );
    }

    #[test]
    fn debug_includes_type_name() {
        let t = TenantId::new("acme");
        assert_eq!(format!("{:?}", t), "TenantId(\"acme\")");
    }

    #[test]
    #[should_panic(expected = "TenantId cannot be empty")]
    fn empty_id_panics_at_construction() {
        let _ = TenantId::new("");
    }

    #[test]
    fn parsing_empty_text_is_refused() {
        assert_eq!("".parse::<ProviderId>(), Err(ParseProviderIdError));
    }

    #[test]
    fn parsing_one_character_is_that_id() {
        assert_eq!(
            "a".parse::<ProviderId>().map(ProviderId::into_inner),
            Ok("a".to_owned())
        );
    }

    #[test]
    fn a_refused_parse_names_the_id() {
        assert_eq!(
            ParseProviderIdError.to_string(),
            "ProviderId cannot be empty"
        );
    }

    #[test]
    fn empty_id_via_serde_is_rejected_not_minted() {
        let r: Result<TenantId, _> = serde_json::from_str("\"\"");
        assert!(r.is_err(), "empty string must not deserialize into an ID");
    }

    /// 2026-09-28T00:00:00Z in epoch ms.
    const MIDNIGHT: Millis = 1_790_553_600_000;

    #[test]
    fn day_of_a_known_v7_is_its_utc_date() {
        // RFC 9562 Appendix A.6: timestamp 0x017F22E279B0 = 2022-02-22T19:22:22Z.
        let uuid = Uuid::parse_str("017F22E2-79B0-7CC3-98C4-DC0C0C07398F").unwrap();
        let id = CallId::try_from(uuid).unwrap();
        assert_eq!(id.day().to_string(), "2022-02-22");
    }

    #[test]
    fn day_flips_exactly_at_utc_midnight() {
        let before = CallId::new_v7(MIDNIGHT - 1).unwrap();
        let at = CallId::new_v7(MIDNIGHT).unwrap();
        assert_eq!(before.day().to_string(), "2026-09-27");
        assert_eq!(at.day().to_string(), "2026-09-28");
        assert!(before.day() < at.day());
    }

    #[test]
    fn utc_day_of_an_instant_agrees_with_the_call_id_minted_at_it() {
        for at in [MIDNIGHT - 1, MIDNIGHT] {
            assert_eq!(UtcDay::of(at), Some(CallId::new_v7(at).unwrap().day()));
        }
        assert_eq!(UtcDay::of(Millis::MAX), None);
    }

    #[test]
    fn new_v7_mints_version_7_and_round_trips() {
        let id = CallId::new_v7(MIDNIGHT).unwrap();
        assert_eq!(id.0.get_version_num(), 7);
        assert_eq!(CallId::try_from(id.0), Ok(id));
    }

    #[test]
    fn session_ids_are_v7_and_parse_back_from_their_text() {
        let id = SessionId::new_v7(MIDNIGHT).unwrap();
        assert_eq!(id.0.get_version_num(), 7);
        assert_eq!(id.day().to_string(), "2026-09-28");
        assert_eq!(id.to_string().parse::<SessionId>(), Ok(id));
    }

    #[test]
    fn a_session_id_round_trips_through_json_as_its_hyphenated_uuid() {
        let id = SessionId::new_v7(MIDNIGHT).unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<SessionId>(&json).unwrap(), id);
    }

    #[test]
    fn a_v4_or_a_free_form_label_is_not_a_session_id() {
        let v4 = Uuid::new_v4().to_string();
        assert!(matches!(
            v4.parse::<SessionId>(),
            Err(SessionIdError::NotV7 { version: 4, .. })
        ));
        assert!(matches!(
            "sess-0000002a".parse::<SessionId>(),
            Err(SessionIdError::NotUuid { .. })
        ));
        assert_eq!("".parse::<SessionId>(), Err(SessionIdError::Empty));
    }

    #[test]
    fn day_is_total_up_to_the_last_v7_millisecond() {
        let last = CallId::new_v7(Millis::try_from(V7_MILLIS_END - 1).unwrap()).unwrap();
        // chrono writes a five-digit year with an explicit sign.
        assert_eq!(last.day().to_string(), "+10889-08-02");
    }

    #[test]
    fn times_outside_the_v7_range_are_refused() {
        assert_eq!(CallId::new_v7(-1), Err(CallIdError::TimeOutOfRange(-1)));
        let end = Millis::try_from(V7_MILLIS_END).unwrap();
        assert_eq!(CallId::new_v7(end), Err(CallIdError::TimeOutOfRange(end)));
    }

    #[test]
    fn a_v4_uuid_is_not_a_call_id() {
        let v4 = Uuid::new_v4();
        assert_eq!(
            CallId::try_from(v4),
            Err(CallIdError::NotV7 {
                uuid: v4,
                version: 4
            })
        );
    }

    #[test]
    fn a_call_id_round_trips_through_json_as_its_hyphenated_uuid() {
        let id = CallId::new_v7(MIDNIGHT).unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", Uuid::from(id).hyphenated()));
        assert_eq!(serde_json::from_str::<CallId>(&json).unwrap(), id);
    }

    #[test]
    fn a_v4_uuid_in_json_is_not_a_call_id() {
        let v4 = Uuid::new_v4();
        let refused = serde_json::from_str::<CallId>(&format!("\"{v4}\"")).unwrap_err();
        assert!(refused.to_string().contains("version 4"), "{refused}");
    }
}
