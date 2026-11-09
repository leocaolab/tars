//! What a read covers: one tenant, a window of time. There is no read of a
//! partitioned log without a [`TenantScope`], and no [`TenantScope`] without
//! a [`TenantId`].
//!
//! ```compile_fail
//! use tars_melt::event::log::{TenantScope, TimeWindow};
//! let scope = TenantScope { window: TimeWindow::all() };
//! ```

use tars_types::{Millis, TenantId, UtcDay};
use thiserror::Error;

/// One tenant's rows over a window. Its fields are private: `new` is the
/// only way to build one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantScope {
    tenant: TenantId,
    window: TimeWindow,
}

impl TenantScope {
    pub fn new(tenant: TenantId, window: TimeWindow) -> Self {
        Self { tenant, window }
    }

    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    pub fn window(&self) -> &TimeWindow {
        &self.window
    }
}

/// A span of time a read covers. Partitions are chosen by [`Self::covers`]
/// (whole UTC days); within them, a row whose time [`Self::admits`] is in
/// the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeWindow(Span);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Span {
    All,
    Since { at: Millis, day: UtcDay },
    Day(UtcDay),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WindowError {
    #[error("{0} ms since the epoch has no UTC calendar day")]
    NoDay(Millis),
}

impl TimeWindow {
    /// Returns the window of every day the tenant has.
    pub fn all() -> Self {
        Self(Span::All)
    }

    /// Returns the window from `at` (epoch ms, inclusive) on.
    pub fn since(at: Millis) -> Result<Self, WindowError> {
        let day = UtcDay::of(at).ok_or(WindowError::NoDay(at))?;
        Ok(Self(Span::Since { at, day }))
    }

    /// Returns the window of one UTC day, the day a call's id says its rows live in.
    pub fn day(day: UtcDay) -> Self {
        Self(Span::Day(day))
    }

    /// Returns whether the partition of `day` holds rows of this window.
    pub fn covers(&self, day: UtcDay) -> bool {
        match self.0 {
            Span::All => true,
            Span::Since { day: first, .. } => day >= first,
            Span::Day(only) => day == only,
        }
    }

    /// Returns whether a row at `at` (epoch ms) of a covered partition is in the window.
    pub fn admits(&self, at: Millis) -> bool {
        self.lower_bound().is_none_or(|since| at >= since)
    }

    /// Returns the first instant (epoch ms) in the window, when it starts at one rather than at a
    /// partition's first row.
    pub fn lower_bound(&self) -> Option<Millis> {
        match self.0 {
            Span::All | Span::Day(_) => None,
            Span::Since { at, .. } => Some(at),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: Millis = 86_400_000;

    fn day(n: Millis) -> UtcDay {
        UtcDay::of(n * DAY_MS).unwrap()
    }

    #[test]
    fn since_covers_its_own_day_and_later_and_admits_from_its_instant() {
        let w = TimeWindow::since(10 * DAY_MS + 5).unwrap();

        assert!(!w.covers(day(9)));
        assert!(w.covers(day(10)));
        assert!(w.covers(day(11)));
        assert!(!w.admits(10 * DAY_MS + 4));
        assert!(w.admits(10 * DAY_MS + 5));
    }

    #[test]
    fn a_day_covers_only_that_day() {
        let w = TimeWindow::day(day(10));

        assert!(!w.covers(day(9)));
        assert!(w.covers(day(10)));
        assert!(!w.covers(day(11)));
    }

    #[test]
    fn an_instant_without_a_day_is_refused() {
        assert_eq!(
            TimeWindow::since(Millis::MAX),
            Err(WindowError::NoDay(Millis::MAX))
        );
    }
}
