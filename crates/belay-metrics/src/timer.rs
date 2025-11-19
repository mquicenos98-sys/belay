//! Scope timers that record their elapsed time into a histogram or summary.

use std::fmt;
use std::time::Duration;

use crate::histogram::Histogram;
use crate::summary::Summary;
use crate::time::TimeSource;

/// Something that accepts observations in seconds and knows what time it is.
///
/// Implemented by [`Histogram`] and [`Summary`]; it is what lets one [`Timer`]
/// type serve both.
pub trait Observe {
    /// Records one observation.
    fn observe(&self, value: f64);

    /// The clock timers started from this target read.
    fn time_source(&self) -> &dyn TimeSource;
}

/// Measures the time from its creation until it is stopped or dropped and
/// records it, in seconds, into its target.
///
/// Timers read the target's [`TimeSource`], so a registry built with
/// [`ManualTime`](crate::ManualTime) produces exact, repeatable durations.
///
/// ```
/// use belay_metrics::{ManualTime, Registry};
/// use std::sync::Arc;
/// use std::time::Duration;
///
/// # fn main() -> belay_metrics::Result<()> {
/// let time = ManualTime::new();
/// let registry = Registry::with_time_source(Arc::new(time.clone()));
/// let latency = registry.histogram("lease_seconds", "Lease latency.", &[0.1, 1.0])?;
///
/// let timer = latency.start_timer();
/// time.advance(Duration::from_millis(250));
/// assert_eq!(timer.stop_and_record(), Duration::from_millis(250));
/// assert_eq!(latency.sum(), 0.25);
/// # Ok(())
/// # }
/// ```
#[must_use = "dropping a timer immediately records a near-zero duration"]
pub struct Timer<O: Observe> {
    target: Option<O>,
    start: Duration,
}

/// A [`Timer`] recording into a [`Histogram`].
pub type HistogramTimer = Timer<Histogram>;

/// A [`Timer`] recording into a [`Summary`].
pub type SummaryTimer = Timer<Summary>;

impl<O: Observe> Timer<O> {
    pub(crate) fn start(target: O) -> Self {
        let start = target.time_source().now();
        Timer {
            target: Some(target),
            start,
        }
    }

    /// Time elapsed so far, without stopping.
    pub fn elapsed(&self) -> Duration {
        match &self.target {
            // A clock that went backwards reads as zero rather than panicking.
            Some(target) => target.time_source().now().saturating_sub(self.start),
            None => Duration::ZERO,
        }
    }

    /// Stops the timer, records the elapsed time and returns it.
    pub fn stop_and_record(mut self) -> Duration {
        self.finish(true)
    }

    /// Stops the timer without recording anything; returns the elapsed time.
    /// Useful when the timed operation failed and only successes are measured.
    pub fn stop_and_discard(mut self) -> Duration {
        self.finish(false)
    }

    fn finish(&mut self, record: bool) -> Duration {
        let elapsed = self.elapsed();
        if let Some(target) = self.target.take() {
            if record {
                target.observe(elapsed.as_secs_f64());
            }
        }
        elapsed
    }
}
