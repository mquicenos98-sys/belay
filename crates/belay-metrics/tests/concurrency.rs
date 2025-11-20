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
