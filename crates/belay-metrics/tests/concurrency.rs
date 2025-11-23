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

#[test]
fn racing_lookups_create_each_child_once() {
    let registry = Registry::new();
    let vec = registry
        .counter_vec("jobs_total", "", &["queue", "outcome"])
        .unwrap();
    let v = vec.clone();
    in_parallel(move |i| {
        for n in 0..ITERATIONS {
            let queue = format!("q{}", n % 16);
            let outcome = if (n + i as u64) % 2 == 0 { "ok" } else { "err" };
            v.with_label_values(&[&queue, outcome]).inc();
        }
    });
    assert_eq!(vec.len(), 32);
    let family = vec.desc().name().to_owned();
    let snapshot = registry.snapshot();
    let total: u64 = snapshot
        .iter()
        .find(|f| f.name == family)
        .unwrap()
        .metrics
        .iter()
        .map(|m| m.value.as_counter().unwrap())
        .sum();
    assert_eq!(total, THREADS as u64 * ITERATIONS);
}

#[test]
fn racing_registrations_share_one_family() {
    let registry = Registry::new();
    let r = registry.clone();
    in_parallel(move |_| {
        for _ in 0..1_000 {
            r.counter_vec("shared_total", "Shared.", &["k"])
                .unwrap()
                .with_label_values(&["v"])
                .inc();
        }
    });
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(
        snapshot[0].metrics[0].value.as_counter(),
        Some(THREADS as u64 * 1_000)
    );
}

#[test]
fn summary_counts_every_observation() {
    let registry = Registry::new();
    let s = registry
        .summary(
            "latency",
            "",
            SummaryOpts::new().quantiles(&[0.5]).reservoir_size(64),
        )
        .unwrap();
    let sc = s.clone();
    in_parallel(move |i| {
        for n in 0..ITERATIONS {
            sc.observe(((n + i as u64) % 100) as f64);
        }
    });
    assert_eq!(s.count(), THREADS as u64 * ITERATIONS);
    let median = s.quantiles()[0].value;
    assert!((0.0..100.0).contains(&median), "median {median}");
}

fn check_histogram(h: &HistogramValue) {
    let mut last = 0;
    for b in &h.buckets {
        assert!(b.cumulative_count >= last, "buckets must be cumulative");
        last = b.cumulative_count;
    }
    assert_eq!(last, h.count, "+Inf bucket must equal the count");
}

#[test]
fn scrapes_during_updates_are_self_consistent() {
    let registry = Registry::new();
    let requests = registry.counter_vec("requests_total", "", &["w"]).unwrap();
    let latency = registry
        .histogram_vec("latency_seconds", "", &["w"], &[0.25, 0.5, 0.75])
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));

    let writers: Vec<_> = (0..4)
        .map(|w| {
            let requests = requests.with_label_values(&[&w.to_string()]);
            let latency = latency.with_label_values(&[&w.to_string()]);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut n = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    requests.inc();
                    latency.observe(f64::from(n % 100) / 100.0);
                    n = n.wrapping_add(1);
                }
            })
        })
        .collect();

    let mut previous_total = 0;
    for _ in 0..200 {
        let mut total = 0;
        for family in registry.snapshot() {
            for metric in &family.metrics {
                match &metric.value {
                    MetricValue::Counter(v) => total += v,
                    MetricValue::Histogram(h) => check_histogram(h),
                    other => panic!("unexpected value {other:?}"),
                }
            }
        }
        assert!(total >= previous_total, "counters went backwards");
        previous_total = total;
        // Rendering takes the same path; make sure it never trips either.
        assert!(registry
            .render_prometheus()
            .contains("# TYPE requests_total counter\n"));
    }
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.join().expect("writer panicked");
    }
}
