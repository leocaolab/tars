//! Writes a `Duration` as a whole number in a config key whose name carries the unit
//! (`initial_backoff_ms`, `cooldown_secs`), the way the provider config writes `timeout_secs`.
//! Writing a duration the unit cannot hold exactly fails instead of rounding it.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serializer, ser::Error as _};

pub(crate) mod millis {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        if !d.subsec_nanos().is_multiple_of(1_000_000) {
            return Err(S::Error::custom(format!(
                "{d:?} is not a whole number of milliseconds"
            )));
        }
        let ms = u64::try_from(d.as_millis())
            .map_err(|_| S::Error::custom(format!("{d:?} does not fit u64 milliseconds")))?;
        s.serialize_u64(ms)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_millis)
    }
}

pub(crate) mod secs {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        if d.subsec_nanos() != 0 {
            return Err(S::Error::custom(format!(
                "{d:?} is not a whole number of seconds"
            )));
        }
        s.serialize_u64(d.as_secs())
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_secs)
    }
}
