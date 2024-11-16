//! Periodic wakeups that stop when a [`Shutdown`] triggers.
//!
//! Deadlines are laid on a fixed grid (`start + n * period`) rather than
//! computed as "now + period" after each tick, so handler run time does not
//! accumulate as drift. When a handler overruns one or more whole periods the
//! missed slots are skipped, not replayed in a burst; [`Tick::missed`] says
//! how many.

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::Shutdown;

/// Shortest period a ticker accepts; a zero period would spin.
const MIN_PERIOD: Duration = Duration::from_millis(1);

/// One firing of a [`Ticker`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tick {
    /// Grid slot of this tick: 0 for the first slot, counting skipped
    /// slots, so it always equals the number of periods since the start.
    pub seq: u64,
    /// When this tick was due.
    pub scheduled: Instant,
    /// Slots skipped since the previous tick because a handler overran.
    pub missed: u64,
}

/// Blocks the calling thread until each period elapses, returning `None` as
/// soon as the shutdown signal triggers.
///
/// ```
/// use belay_common::sync::{Shutdown, Ticker};
/// use std::time::Duration;
///
/// let shutdown = Shutdown::new();
/// let mut runs = 0;
/// for _tick in Ticker::new(Duration::from_millis(1), shutdown.clone()) {
///     runs += 1;
///     if runs == 3 {
///         shutdown.trigger();
///     }
/// }
/// assert_eq!(runs, 3);
/// ```
#[derive(Debug)]
pub struct Ticker {
    period: Duration,
    next: Instant,
    next_seq: u64,
    shutdown: Shutdown,
}

impl Ticker {
    /// A ticker whose first tick is one `period` from now. Periods below
    /// one millisecond are raised to one millisecond.
    #[must_use]
    pub fn new(period: Duration, shutdown: Shutdown) -> Ticker {
        let period = period.max(MIN_PERIOD);
        Ticker {
            period,
            next: Instant::now() + period,
            next_seq: 0,
            shutdown,
        }
    }

    /// A ticker whose first tick is due immediately.
    #[must_use]
    pub fn starting_now(period: Duration, shutdown: Shutdown) -> Ticker {
        let mut ticker = Ticker::new(period, shutdown);
        ticker.next = Instant::now();
        ticker
    }

    /// The tick interval.
    #[must_use]
    pub fn period(&self) -> Duration {
        self.period
    }

    /// Waits for the next tick. Returns `None` once shutdown has triggered.
    pub fn wait(&mut self) -> Option<Tick> {
        if self.shutdown.is_triggered() || self.shutdown.wait_deadline(self.next) {
            return None;
        }
        let (missed, scheduled) = latest_slot(self.next, self.period, Instant::now());
        let tick = Tick {
            seq: self.next_seq + missed,
            scheduled,
            missed,
        };
        self.next_seq = tick.seq + 1;
        self.next = scheduled + self.period;
        Some(tick)
    }
}

impl Iterator for Ticker {
    type Item = Tick;

    fn next(&mut self) -> Option<Tick> {
        self.wait()
    }
}

/// Given a slot `due` that has been reached at `now`, returns how many
/// further whole slots have also passed and the latest slot not after `now`.
fn latest_slot(due: Instant, period: Duration, now: Instant) -> (u64, Instant) {
    let late = now.saturating_duration_since(due);
    let missed = u64::try_from(late.as_nanos() / period.as_nanos()).unwrap_or(u64::MAX);
    let offset = period.saturating_mul(u32::try_from(missed).unwrap_or(u32::MAX));
    (missed, due + offset)
}
