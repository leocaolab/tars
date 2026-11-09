//! Defines [`Schedule`], the retry policy of one durable call as a value.
use std::time::Duration;

/// How many times to try one call, and how long to wait between tries.
///
/// # Why this is not the runner's `failure_limit`
///
/// The runner retries the whole task: a fifty-call agent that hits a 429 on call forty
/// would spend a task attempt on it, and two flaky calls would share one budget. The two
/// cover different failures:
///
/// | | scope | what it survives |
/// |---|---|---|
/// | `QueueConfig::failure_limit` | the whole task | a crash, a kill, a wedge |
/// | `Schedule` | **one call** | a 429, a 503, a dropped connection |
///
/// # Where the waiting happens
///
/// Inside the call, before its result is journaled: the journal records only the attempt
/// that worked or the failure that ended it, so a resumed run reads that back without
/// retrying, and sleeping or jitter cannot affect replay.
///
/// **The whole schedule runs inside one lease.** A total backoff longer than the runner's
/// lease TTL lets `reclaim_expired` re-arm the task while this attempt still sleeps, and
/// two copies run at once. Check [`at_most`](Self::at_most) against the lease; for waits
/// of minutes, park the task instead (`ask`'s deadline, a task's `wake_at`).
///
/// # What is retried
///
/// Only what [`Reason::is_retryable`](crate::Reason::is_retryable) admits; a `Denied`, a
/// `BadGoal` or a `Defect` fails identically on re-run.
#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    attempts: u32,
    base: Duration,
    factor: f64,
    cap: Duration,
    jitter: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        Self::once()
    }
}

impl Schedule {
    pub fn once() -> Self {
        Self {
            attempts: 1,
            base: Duration::ZERO,
            factor: 1.0,
            cap: Duration::ZERO,
            jitter: false,
        }
    }

    /// Returns a schedule of `attempts` tries in total, waiting `delay` between each.
    ///
    /// `attempts` counts the first try: `fixed(d, 3)` calls at most three times and waits
    /// twice. Zero is treated as one.
    pub fn fixed(delay: Duration, attempts: u32) -> Self {
        Self {
            attempts: attempts.max(1),
            base: delay,
            factor: 1.0,
            cap: Duration::MAX,
            jitter: false,
        }
    }

    /// Returns a schedule of `attempts` tries in total, the wait doubling from `base`; zero is
    /// treated as one, as in [`Self::fixed`].
    pub fn exponential(base: Duration, attempts: u32) -> Self {
        Self {
            attempts: attempts.max(1),
            base,
            factor: 2.0,
            cap: Duration::MAX,
            jitter: false,
        }
    }

    pub fn capped_at(mut self, cap: Duration) -> Self {
        self.cap = cap;
        self
    }

    /// Asks the caller to spread each wait randomly over `[0, delay]` (full jitter).
    ///
    /// A rate limit hits a fan-out all at once, and an unjittered backoff sends every
    /// agent back at the same instant to be refused again.
    pub fn jittered(mut self) -> Self {
        self.jitter = true;
        self
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn wants_jitter(&self) -> bool {
        self.jitter
    }

    /// Returns the nominal wait before try number `next`, counting from 1, so the wait
    /// before the second try is `delay_before(2)`; the caller applies jitter.
    pub fn delay_before(&self, next: u32) -> Duration {
        if next <= 1 || self.base.is_zero() {
            return Duration::ZERO;
        }
        let steps = (next - 2) as i32;
        let scaled = self.base.as_secs_f64() * self.factor.powi(steps);
        let d = Duration::try_from_secs_f64(scaled).unwrap_or(self.cap);
        d.min(self.cap)
    }

    /// Returns the longest this schedule can spend waiting, not counting the calls; it must
    /// stay under the lease TTL and the step budget. A total past `Duration::MAX` saturates
    /// there, since the attempts and delays may come from a user.
    pub fn at_most(&self) -> Duration {
        (2..=self.attempts)
            .map(|n| self.delay_before(n))
            .fold(Duration::ZERO, Duration::saturating_add)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_most_saturates_instead_of_overflowing() {
        let long = Schedule::exponential(Duration::from_secs(1), 100);
        assert_eq!(long.at_most(), Duration::MAX);
        let huge = Schedule::fixed(Duration::MAX, 3);
        assert_eq!(huge.at_most(), Duration::MAX);
    }

    #[test]
    fn once_never_waits_and_never_retries() {
        let s = Schedule::once();
        assert_eq!(s.attempts(), 1);
        assert_eq!(s.at_most(), Duration::ZERO);
    }

    #[test]
    fn exponential_doubles_from_the_base_and_honours_the_cap() {
        let s = Schedule::exponential(Duration::from_millis(100), 5)
            .capped_at(Duration::from_millis(250));
        assert_eq!(
            s.delay_before(1),
            Duration::ZERO,
            "no wait before the FIRST try"
        );
        assert_eq!(s.delay_before(2), Duration::from_millis(100));
        assert_eq!(s.delay_before(3), Duration::from_millis(200));
        assert_eq!(s.delay_before(4), Duration::from_millis(250), "capped");
        assert_eq!(s.delay_before(5), Duration::from_millis(250));
        assert_eq!(s.at_most(), Duration::from_millis(800));
    }

    #[test]
    fn attempts_counts_the_first_try_and_zero_counts_as_one() {
        assert_eq!(Schedule::fixed(Duration::from_secs(1), 3).attempts(), 3);
        assert_eq!(
            Schedule::fixed(Duration::from_secs(1), 3).at_most(),
            Duration::from_secs(2),
            "three tries wait twice"
        );
        assert_eq!(Schedule::fixed(Duration::from_secs(1), 1).attempts(), 1);
        assert_eq!(Schedule::fixed(Duration::from_secs(1), 0).attempts(), 1);
    }
}
