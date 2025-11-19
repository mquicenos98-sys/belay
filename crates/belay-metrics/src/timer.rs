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

impl<O: Observe> Drop for Timer<O> {
    fn drop(&mut self) {
        self.finish(true);
    }
}

impl<O: Observe> fmt::Debug for Timer<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Timer")
            .field("elapsed", &self.elapsed())
            .field("armed", &self.target.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::{Histogram, ManualTime};
    use std::sync::Arc;
    use std::time::Duration;

    fn setup() -> (ManualTime, Histogram) {
        let time = ManualTime::new();
        let h = Histogram::with_time_source(&[0.5, 1.0], Arc::new(time.clone())).unwrap();
        (time, h)
    }

    #[test]
    fn drop_records_exactly_once() {
        let (time, h) = setup();
        {
            let _timer = h.start_timer();
            time.advance(Duration::from_millis(750));
        }
        assert_eq!(h.count(), 1);
        assert_eq!(h.sum(), 0.75);
    }

    #[test]
    fn stop_and_record_does_not_record_again_on_drop() {
        let (time, h) = setup();
        let timer = h.start_timer();
        time.advance(Duration::from_secs(2));
        assert_eq!(timer.stop_and_record(), Duration::from_secs(2));
        assert_eq!(h.count(), 1);
        assert_eq!(h.snapshot().buckets[1].cumulative_count, 0);
    }

    #[test]
    fn stop_and_discard_records_nothing() {
        let (time, h) = setup();
        let timer = h.start_timer();
        time.advance(Duration::from_millis(10));
        assert_eq!(timer.elapsed(), Duration::from_millis(10));
        assert_eq!(timer.stop_and_discard(), Duration::from_millis(10));
        assert_eq!(h.count(), 0);
    }

    #[test]
    fn clock_going_backwards_records_zero() {
        let (time, h) = setup();
        time.set(Duration::from_secs(10));
        let timer = h.start_timer();
        time.set(Duration::from_secs(3));
        assert_eq!(timer.stop_and_record(), Duration::ZERO);
        assert_eq!(h.sum(), 0.0);
    }

    #[test]
    fn time_closure_returns_the_result_and_records() {
        let (time, h) = setup();
        let out = h.time(|| {
            time.advance(Duration::from_millis(100));
            "done"
        });
        assert_eq!(out, "done");
        assert_eq!(h.sum(), 0.1);
    }
}
