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
