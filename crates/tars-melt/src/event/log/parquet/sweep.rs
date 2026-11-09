//! Pure core of the past-day sweep: no row is sealed into a past day any
//! more, so no seal triggers its merge into one file. The writer sweeps
//! every past partition instead — when its sink opens, and on the first seal
//! of each new day. Days arrive as arguments; nothing here does IO.

use tars_types::UtcDay;

use super::partition::PartitionDir;

/// Returns the day to sweep for: `today`, unless this writer already swept for it
/// (or for a later day — a clock stepped back sweeps nothing). No known
/// `today`, no sweep.
pub(super) fn sweep_day(swept: Option<UtcDay>, today: Option<UtcDay>) -> Option<UtcDay> {
    today.filter(|today| swept.is_none_or(|swept| swept < *today))
}

/// Returns the partitions a sweep for `today` merges: those of the days before it.
pub(super) fn past_partitions(listed: Vec<PartitionDir>, today: UtcDay) -> Vec<PartitionDir> {
    listed.into_iter().filter(|p| p.day() < today).collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const DAY_MS: i64 = 86_400_000;

    fn day(n: i64) -> UtcDay {
        UtcDay::of(n * DAY_MS).unwrap()
    }

    #[test]
    fn a_writer_sweeps_once_per_new_day() {
        assert_eq!(sweep_day(None, Some(day(5))), Some(day(5)), "on open");
        assert_eq!(sweep_day(Some(day(5)), Some(day(5))), None, "same day");
        assert_eq!(
            sweep_day(Some(day(5)), Some(day(6))),
            Some(day(6)),
            "past midnight"
        );
        assert_eq!(
            sweep_day(Some(day(6)), Some(day(5))),
            None,
            "clock stepped back"
        );
        assert_eq!(sweep_day(None, None), None, "no clock day");
    }

    #[test]
    fn only_the_days_before_today_are_swept() {
        let listed = |n: i64| PartitionDir::new(Path::new("t"), day(n));
        let swept = past_partitions(vec![listed(3), listed(5), listed(4), listed(6)], day(5));
        assert_eq!(swept, vec![listed(3), listed(4)]);
    }
}
