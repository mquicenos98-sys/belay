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

/// Spawns a thread named `name` that calls `f` on every tick until
/// `shutdown` triggers.
///
/// A panic in `f` is caught and logged at error level, and the thread keeps
/// ticking: a background chore such as lease expiry must not silently stop
/// because of one bad iteration.
pub fn spawn_periodic<F>(
    name: &str,
    period: Duration,
    shutdown: Shutdown,
    mut f: F,
) -> io::Result<JoinHandle<()>>
where
    F: FnMut(Tick) + Send + 'static,
{
    let thread_name = name.to_owned();
    thread::Builder::new().name(thread_name.clone()).spawn(move || {
        for tick in Ticker::new(period, shutdown) {
            if panic::catch_unwind(AssertUnwindSafe(|| f(tick))).is_err() {
                crate::error!("periodic task panicked"; thread = thread_name.as_str(), seq = tick.seq);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn on_time_tick_skips_nothing() {
        let due = Instant::now();
        let period = Duration::from_millis(10);
        assert_eq!(latest_slot(due, period, due), (0, due));
        let slightly_late = due + Duration::from_millis(9);
        assert_eq!(latest_slot(due, period, slightly_late), (0, due));
    }

    #[test]
    fn overrun_skips_whole_periods_and_stays_on_the_grid() {
        let due = Instant::now();
        let period = Duration::from_millis(10);
        let now = due + Duration::from_millis(25);
        assert_eq!(
            latest_slot(due, period, now),
            (2, due + Duration::from_millis(20))
        );
        let now = due + Duration::from_millis(30);
        assert_eq!(
            latest_slot(due, period, now),
            (3, due + Duration::from_millis(30))
        );
    }

    #[test]
    fn ticks_follow_the_grid_without_drift() {
        let shutdown = Shutdown::new();
        let period = Duration::from_millis(2);
        let start = Instant::now();
        let mut ticker = Ticker::new(period, shutdown.clone());
        let mut first: Option<Tick> = None;
        let mut ticks = 0;
        while let Some(tick) = ticker.wait() {
            assert!(Instant::now() >= tick.scheduled, "never fires early");
            // Slot n is due exactly n periods after slot 0, however long
            // the previous iterations took.
            let base = *first.get_or_insert(tick);
            let slots = (tick.seq - base.seq) as u32;
            assert_eq!(tick.scheduled, base.scheduled + period * slots);
            assert!(base.scheduled >= start + period * (base.seq as u32 + 1));
            ticks += 1;
            if ticks == 3 {
                shutdown.trigger();
            }
        }
        assert_eq!(ticks, 3);
        assert!(start.elapsed() >= period * 3);
    }

    #[test]
    fn slow_handler_reports_missed_ticks() {
        let shutdown = Shutdown::new();
        let mut ticker = Ticker::starting_now(Duration::from_millis(2), shutdown.clone());
        let first = ticker.wait().expect("first tick");
        assert_eq!((first.seq, first.missed), (0, 0));
        thread::sleep(Duration::from_millis(9));
        let second = ticker.wait().expect("second tick");
        assert!(second.missed >= 3, "{second:?}");
        assert_eq!(second.seq, 1 + second.missed);
    }

    #[test]
    fn shutdown_interrupts_a_long_wait() {
        let shutdown = Shutdown::new();
        let mut ticker = Ticker::new(Duration::from_secs(3600), shutdown.clone());
        let trigger = {
            let s = shutdown.clone();
            thread::spawn(move || {
                s.trigger();
            })
        };
        let start = Instant::now();
        assert_eq!(ticker.wait(), None);
        assert!(start.elapsed() < Duration::from_secs(60));
        trigger.join().expect("trigger thread panicked");
        assert_eq!(ticker.next(), None, "stays stopped");
    }

    #[test]
    fn starting_now_fires_immediately_and_zero_period_is_clamped() {
        let shutdown = Shutdown::new();
        let mut ticker = Ticker::starting_now(Duration::from_secs(3600), shutdown.clone());
        let start = Instant::now();
        assert!(ticker.wait().is_some());
        assert!(start.elapsed() < Duration::from_secs(60));
        assert_eq!(Ticker::new(Duration::ZERO, shutdown).period(), MIN_PERIOD);
    }

    #[test]
    fn spawn_periodic_survives_panics_and_stops_on_shutdown() {
        let shutdown = Shutdown::new();
        let calls = Arc::new(AtomicU64::new(0));
        let handle = {
            let calls = Arc::clone(&calls);
            let s = shutdown.clone();
            spawn_periodic(
                "ticker-test",
                Duration::from_millis(1),
                shutdown.clone(),
                move |_| {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 1 {
                        panic!("first iteration fails on purpose");
                    }
                    if n == 3 {
                        s.trigger();
                    }
                },
            )
            .expect("spawning ticker thread")
        };
        handle.join().expect("ticker thread exits cleanly");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
