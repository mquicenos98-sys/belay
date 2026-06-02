//! Throughput and latency benchmarks for belay's components, run by the
//! `belay-bench` binary.
//!
//! ```text
//! cargo run -p belay-bench --release -- all --quick
//! cargo run -p belay-bench --release -- store-write --filter sync=always --json
//! ```
//!
//! # Scenarios
//!
//! | scenario | measures |
//! |---|---|
//! | `store-write` | `LogKv` writes on a real directory by sync policy, batch size, value size and writer threads |
//! | `store-recover` | opening a store: replaying the log versus loading a snapshot, by data size |
//! | `queue-embedded` | `QueueEngine` enqueue, lease and complete on `MemKv` and `LogKv`, by thread count |
//! | `queue-http` | the same operations through `belay-server` and `belay-client` over loopback |
//! | `worker-e2e` | the worker runtime draining a backlog, and idle pickup latency |
//! | `cron` | `CronExpr::next_after` in UTC and in daylight-saving zones |
//! | `json` | serializing, parsing and decoding job documents of three sizes |
//!
//! Each scenario runs a matrix of *cases*, each identified by parameters
//! (`sync=always batch=16 value=128B threads=1`) that `--filter` can
//! select. `--quick` shrinks the matrix and iteration counts for CI;
//! `--smoke` runs every case once or twice to check that it works.
//!
//! # Method
//!
//! A case makes untimed warm-up calls, then measured calls
//! ([`harness::measure`]). Every measured call is timed on its own and
//! recorded in a log-linear [`histogram::Histogram`] whose percentiles are
//! within 0.8% of the exact values; throughput is the work done divided by
//! the wall-clock time of the measured phase. Multi-threaded cases
//! ([`harness::measure_parallel`]) hold every thread at a barrier until all
//! have warmed up, so their measured phases overlap. All synthetic data
//! (keys, values, documents, instants) comes from a seeded random stream:
//! `--seed` reproduces a run's inputs exactly.
//!
//! Results print as one table per scenario (rate, MB/s where bytes are
//! counted, p50/p90/p99/p99.9/max latency, scenario notes), or as one JSON
//! document with `--json`. A header states the build profile, CPU count
//! and the cost of reading the clock, which every latency includes.
//!
//! # Limitations
//!
//! - Numbers from a debug build are not representative; the header warns.
//! - Sub-microsecond operations (`cron`, small `json` documents) include
//!   the cost of one `Instant::now()` per call, tens of nanoseconds on
//!   most systems.
//! - `store-recover` measures recovery with the files in the OS page
//!   cache (one unrecorded open warms it); a cold start after a reboot
//!   reads from the device and is slower.
//! - There is no statistical comparison between runs: compare the JSON
//!   of two runs yourself, and repeat a run before trusting a small
//!   difference.
//! - The HTTP scenarios run client and server on one machine, so they
//!   compete for its CPUs.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

use std::io::Write;

use belay_common::log::{set_max_level, LevelFilter};

pub mod args;

mod error;

pub mod harness;

pub mod histogram;

pub mod report;

pub mod scenarios;

pub mod session;

pub mod units;

pub use args::{Command, Scale, Settings};
pub use error::{Error, Result};
pub use report::{Environment, Report};
pub use session::{CaseResult, Session};

/// This crate's version, which is belay's.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runs the scenarios `settings` selects, writing a progress line per
/// case to `progress`. A scenario that fails is recorded in
/// [`Report::failures`] and the run continues with the next one.
pub fn execute(
    environment: Environment,
    settings: Settings,
    progress: Option<&mut dyn Write>,
) -> Report {
    let mut session = Session::new(settings.clone());
    if let Some(out) = progress {
        session = session.with_progress(out);
    }
    let mut failures = Vec::new();
    for &name in &settings.scenarios {
        match scenarios::find(name) {
            Some(scenario) => {
                if let Err(e) = (scenario.run)(&mut session) {
                    failures.push((scenario.name, e));
                }
            }
            None => failures.push((name, Error::Usage(format!("unknown scenario {name:?}")))),
        }
    }
    Report {
        environment,
        settings,
        results: session.into_results(),
        failures,
    }
}
