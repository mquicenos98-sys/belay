//! Measuring loops: warm-up calls, then measured calls each timed into a
//! [`Histogram`], on one thread or several started together.
//!
//! An operation is a closure called with a sequence number and returning
//! the [`Work`] it did (items, bytes). The harness times every call, adds
//! up the work, and measures the wall-clock time of the measured phase;
//! throughput is work divided by that wall-clock time, so with several
//! threads it is the aggregate rate.

use std::fmt;
use std::ops::Range;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use crate::histogram::Histogram;
use crate::{Error, Result};

/// The parameters that identify a case within a scenario, in a fixed
/// order: `sync=always batch=16 value=128B`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params(Vec<(&'static str, String)>);

impl Params {
    /// No parameters.
    #[must_use]
    pub fn new() -> Params {
        Params::default()
    }

    /// Appends `key=value`.
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl fmt::Display) -> Params {
        self.0.push((key, value.to_string()));
        self
    }

    /// The value of `key`, if present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The parameters in order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> + '_ {
        self.0.iter().map(|(k, v)| (*k, v.as_str()))
    }
}

impl fmt::Display for Params {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (key, value)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{key}={value}")?;
        }
        Ok(())
    }
}

/// What one call of an operation did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Work {
    /// Items processed (keys written, jobs enqueued, documents parsed).
    pub items: u64,
    /// Payload bytes processed, for MB/s; 0 when not meaningful.
    pub bytes: u64,
}

impl Work {
    /// One item, no byte count.
    pub const ONE: Work = Work { items: 1, bytes: 0 };

    /// `items` items, no byte count.
    #[must_use]
    pub fn items(items: u64) -> Work {
        Work { items, bytes: 0 }
    }

    /// Sets the byte count.
    #[must_use]
    pub fn with_bytes(self, bytes: u64) -> Work {
        Work { bytes, ..self }
    }
}

/// How many calls to make: `warmup` untimed ones, then `iterations`
/// measured ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Calls made before measuring, to fill caches, grow tables and start
    /// connections.
    pub warmup: u64,
    /// Calls measured.
    pub iterations: u64,
}

impl Plan {
    /// `iterations` measured calls after a tenth as many warm-up calls.
    #[must_use]
    pub fn new(iterations: u64) -> Plan {
        Plan {
            warmup: iterations / 10,
            iterations,
        }
    }

    /// Sets the number of warm-up calls.
    #[must_use]
    pub fn warmup(self, warmup: u64) -> Plan {
        Plan { warmup, ..self }
    }

    /// Warm-up plus measured calls.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.warmup + self.iterations
    }
}

/// The outcome of a measured phase.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// Measured calls.
    pub iterations: u64,
    /// Items processed by them.
    pub items: u64,
    /// Bytes processed by them.
    pub bytes: u64,
    /// Wall-clock time of the measured phase; throughput divides by this.
    pub elapsed: Duration,
    /// Sum of the recorded call latencies.
    pub busy: Duration,
    /// Latency of each recorded call, in nanoseconds. Empty when a case
    /// measures throughput only.
    pub latency: Histogram,
}

impl Sample {
    /// An empty sample.
    #[must_use]
    pub fn new() -> Sample {
        Sample::default()
    }

    /// Records one call that took `latency` and did `work`.
    pub fn record(&mut self, latency: Duration, work: Work) {
        self.latency.record_duration(latency);
        self.busy += latency;
        self.add(work);
    }

    /// Counts a call without a latency (for throughput-only cases).
    pub fn add(&mut self, work: Work) {
        self.iterations += 1;
        self.items += work.items;
        self.bytes += work.bytes;
    }

    /// Adds another thread's sample. The elapsed time becomes the longer
    /// of the two; callers timing a parallel phase overwrite it.
    pub fn merge(&mut self, other: &Sample) {
        self.iterations += other.iterations;
        self.items += other.items;
        self.bytes += other.bytes;
        self.elapsed = self.elapsed.max(other.elapsed);
        self.busy += other.busy;
        self.latency.merge(&other.latency);
    }

    /// Items per second of wall-clock time (0 if no time elapsed).
    #[must_use]
    pub fn rate(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            self.items as f64 / secs
        } else {
            0.0
        }
    }
}

/// Runs `op` on the calling thread: `plan.warmup` untimed calls, then
/// `plan.iterations` timed ones. Sequence numbers run from 0 across both
/// phases, so they can index data prepared in advance or make keys unique.
///
/// ```
/// use belay_bench::harness::{measure, Plan, Work};
///
/// let mut sum = 0u64;
/// let sample = measure(Plan::new(1_000), |seq| {
///     sum = sum.wrapping_add(seq);
///     Ok(Work::ONE)
/// })?;
/// assert_eq!(sample.iterations, 1_000);
/// assert_eq!(sample.latency.count(), 1_000);
/// # Ok::<(), belay_bench::Error>(())
/// ```
pub fn measure<F>(plan: Plan, mut op: F) -> Result<Sample>
where
    F: FnMut(u64) -> Result<Work>,
{
    for seq in 0..plan.warmup {
        op(seq)?;
    }
    timed(&mut op, plan.warmup..plan.total())
}

/// Runs the same plan split over `threads` threads. Each thread builds
/// its operation with `make(thread_index)` and warms up; the measured
/// phase starts on all threads at once when the last one is ready, and
/// its wall-clock time runs until the last one finishes. Sequence numbers
/// are unique across all threads and both phases.
pub fn measure_parallel<M, F>(plan: Plan, threads: usize, make: M) -> Result<Sample>
where
    M: Fn(usize) -> Result<F> + Sync,
    F: FnMut(u64) -> Result<Work>,
{
    let threads = threads.max(1);
    // The calling thread waits too, and starts the clock on release.
    let ready = Barrier::new(threads + 1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let share = Plan {
                    warmup: split(plan.warmup, threads, t),
                    iterations: split(plan.iterations, threads, t),
                };
                let (ready, make, start) = (&ready, &make, t as u64 * share.total());
                scope.spawn(move || run_share(ready, make, t, start, share))
            })
            .collect();
        ready.wait();
        let started = Instant::now();
        let mut total = Sample::new();
        let mut first_error = None;
        for handle in handles {
            let outcome = handle
                .join()
                .unwrap_or_else(|_| Err(Error::Invalid("a benchmark thread panicked".into())));
            match outcome {
                Ok(sample) => total.merge(&sample),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        total.elapsed = started.elapsed();
        first_error.map_or(Ok(total), Err)
    })
}

fn run_share<M, F>(
    ready: &Barrier,
    make: &M,
    thread: usize,
    first: u64,
    share: Plan,
) -> Result<Sample>
where
    M: Fn(usize) -> Result<F>,
    F: FnMut(u64) -> Result<Work>,
{
    // Setup must reach the barrier whatever happens, or every other
    // thread would wait there forever: errors and panics are held until
    // after it.
    let prepared = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut op = make(thread)?;
        for seq in first..first + share.warmup {
            op(seq)?;
        }
        Ok(op)
    }));
    ready.wait();
    let mut op = prepared.unwrap_or_else(|_| {
        Err(Error::Invalid(
            "a benchmark thread panicked during setup".into(),
        ))
    })?;
    timed(&mut op, first + share.warmup..first + share.total())
}

/// Calls `op` for each sequence number, timing every call and the whole.
fn timed<F>(op: &mut F, seqs: Range<u64>) -> Result<Sample>
where
    F: FnMut(u64) -> Result<Work>,
{
    let mut sample = Sample::new();
    let started = Instant::now();
    for seq in seqs {
        let call = Instant::now();
        let work = op(seq)?;
        sample.record(call.elapsed(), work);
    }
    sample.elapsed = started.elapsed();
    Ok(sample)
}

/// Thread `index`'s share of `n` items split `parts` ways as evenly as
/// possible, the remainder going to the first threads.
fn split(n: u64, parts: usize, index: usize) -> u64 {
    let parts = parts as u64;
    let index = index as u64;
    n / parts + u64::from(index < n % parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn params_render_in_insertion_order() {
        let p = Params::new().with("sync", "always").with("batch", 16);
        assert_eq!(p.to_string(), "sync=always batch=16");
        assert_eq!(p.get("batch"), Some("16"));
        assert_eq!(p.get("value"), None);
        assert_eq!(Params::new().to_string(), "");
    }

    #[test]
    fn measure_times_only_the_measured_calls() {
        let plan = Plan::new(20).warmup(5);
        assert_eq!((Plan::new(1_000).warmup, plan.total()), (100, 25));
        let mut seen = Vec::new();
        let sample = measure(plan, |seq| {
            seen.push(seq);
            Ok(Work::items(2).with_bytes(10))
        })
        .unwrap();
        assert_eq!(seen, (0..25).collect::<Vec<_>>());
        assert_eq!(
            (sample.iterations, sample.items, sample.bytes),
            (20, 40, 200)
        );
        assert_eq!(sample.latency.count(), 20);
        assert!(sample.busy <= sample.elapsed);
    }

    #[test]
    fn measure_stops_at_the_first_error() {
        let mut calls = 0;
        let err = measure(Plan::new(10).warmup(0), |seq| {
            calls += 1;
            match seq {
                3 => Err(Error::Invalid("queue ran dry".into())),
                _ => Ok(Work::ONE),
            }
        })
        .unwrap_err();
        assert_eq!((err.to_string().as_str(), calls), ("queue ran dry", 4));
    }

    #[test]
    fn parallel_setup_errors_are_reported_without_hanging() {
        let measured = AtomicU64::new(0);
        let counter = &measured;
        let err = measure_parallel(Plan::new(40).warmup(0), 3, |t| {
            if t == 1 {
                return Err(Error::Invalid("thread 1 could not connect".into()));
            }
            Ok(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
                Ok(Work::ONE)
            })
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "thread 1 could not connect");
        // The healthy threads still ran their shares (14 and 13 calls).
        assert_eq!(measured.load(Ordering::Relaxed), 27);
    }

    #[test]
    fn panicking_threads_are_errors_not_deadlocks() {
        let err = measure_parallel(Plan::new(8), 2, |t| {
            assert!(t != 0, "setup of thread 0 panics");
            Ok(|_| Ok(Work::ONE))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "a benchmark thread panicked during setup");
        let err = measure_parallel(Plan::new(8).warmup(0), 2, |_| {
            Ok(|seq| {
                assert!(seq != 5, "call 5 panics");
                Ok(Work::ONE)
            })
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "a benchmark thread panicked");
    }

    #[test]
    fn work_is_split_evenly_with_the_remainder_first() {
        let shares: Vec<u64> = (0..4).map(|t| split(10, 4, t)).collect();
        assert_eq!(shares, [3, 3, 2, 2]);
        assert_eq!(split(0, 3, 2), 0);
    }

    #[test]
    fn merged_samples_add_up_and_keep_the_longest_time() {
        let mut a = Sample::new();
        a.record(Duration::from_micros(5), Work::items(3));
        a.elapsed = Duration::from_millis(2);
        let mut b = Sample::new();
        b.add(Work::ONE.with_bytes(7));
        b.elapsed = Duration::from_millis(3);
        a.merge(&b);
        assert_eq!((a.iterations, a.items, a.bytes), (2, 4, 7));
        assert_eq!(
            (a.elapsed, a.latency.count()),
            (Duration::from_millis(3), 1)
        );
        assert!((a.rate() - 4.0 / 0.003).abs() < 1e-6);
        assert_eq!(Sample::new().rate(), 0.0);
    }
}
