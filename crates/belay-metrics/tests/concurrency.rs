//! Many threads updating, creating and scraping metrics at once. Every
//! assertion is exact: lost updates, double-created children or torn
//! histogram snapshots all show up as wrong numbers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use belay_metrics::{HistogramValue, MetricValue, Registry, SummaryOpts};

const THREADS: usize = 8;

const ITERATIONS: u64 = 20_000;

/// Runs `work(thread_index)` on `THREADS` threads released together.
fn in_parallel(work: impl Fn(usize) + Send + Sync + 'static) {
    let work = Arc::new(work);
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let work = Arc::clone(&work);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                work(i);
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker thread panicked");
    }
}

#[test]
fn counter_increments_are_never_lost() {
    let registry = Registry::new();
    let counter = registry.counter("hits_total", "").unwrap();
    let c = counter.clone();
    in_parallel(move |i| {
        for _ in 0..ITERATIONS {
            c.inc();
        }
        c.inc_by(i as u64);
    });
    let extra: u64 = (0..THREADS as u64).sum();
    assert_eq!(counter.get(), THREADS as u64 * ITERATIONS + extra);
}

#[test]
fn gauge_adds_and_subs_balance_out() {
    let registry = Registry::new();
    let gauge = registry.gauge("in_flight", "").unwrap();
    let g = gauge.clone();
    in_parallel(move |i| {
        for _ in 0..ITERATIONS {
            if i % 2 == 0 {
                g.add(3.0);
            } else {
                g.sub(1.0);
            }
            let _guard = g.track_in_flight();
        }
    });
    let half = (THREADS / 2) as f64 * ITERATIONS as f64;
    assert_eq!(gauge.get(), half * 3.0 - half);
}

#[test]
fn histogram_buckets_and_sum_are_exact() {
    let registry = Registry::new();
    let h = registry
        .histogram("sizes", "", &[1.0, 10.0, 100.0])
        .unwrap();
    let hc = h.clone();
    in_parallel(move |i| {
        // Thread i always observes the same value, so expected buckets are
        // known exactly; integers keep the f64 sum exact.
        let v = [0.0, 5.0, 50.0, 500.0][i % 4];
        for _ in 0..ITERATIONS {
            hc.observe(v);
        }
    });
    let per_value = (THREADS / 4) as u64 * ITERATIONS;
    let snap = h.snapshot();
    let cumulative: Vec<u64> = snap.buckets.iter().map(|b| b.cumulative_count).collect();
    assert_eq!(
        cumulative,
        [per_value, 2 * per_value, 3 * per_value, 4 * per_value]
    );
    assert_eq!(snap.sum, per_value as f64 * 555.0);
}
