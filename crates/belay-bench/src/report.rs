//! Presenting results: the environment header, one aligned table per
//! scenario, and the `--json` document.

use std::fmt;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use belay_common::text::{Align, Table};
use belay_common::{system_clock, Timestamp};
use belay_json::{ObjectBuilder, Value};

use crate::args::Settings;
use crate::histogram::Summary;
use crate::session::CaseResult;
use crate::units::{format_count, format_elapsed, format_mb_per_sec, format_nanos, format_rate};
use crate::{scenarios, Error};

/// Version of the `--json` document layout; bumped when fields change
/// meaning or disappear, not when fields are added.
pub const JSON_FORMAT: u64 = 1;

/// Where the numbers were taken.
#[derive(Clone, Debug, PartialEq)]
pub struct Environment {
    /// `std::env::consts::OS`.
    pub os: &'static str,
    /// `std::env::consts::ARCH`.
    pub arch: &'static str,
    /// `std::thread::available_parallelism`, 0 if unknown.
    pub cpus: usize,
    /// `release` or `debug` (whether debug assertions are on).
    pub profile: &'static str,
    /// belay version.
    pub version: &'static str,
    /// When the run started.
    pub started_at: Timestamp,
    /// Average cost of one `Instant::now()`, included in every recorded
    /// latency.
    pub clock_cost: Duration,
    /// Smallest step `Instant` was seen to advance by; latencies are
    /// multiples of it (100ns on Windows).
    pub clock_resolution: Duration,
    /// Directory the store scenarios write under.
    pub store_dir: PathBuf,
}

impl Environment {
    /// Looks at the machine and build; `dir` is the `--dir` setting.
    #[must_use]
    pub fn detect(dir: Option<&Path>) -> Environment {
        Environment {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            cpus: std::thread::available_parallelism().map_or(0, |n| n.get()),
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            version: crate::VERSION,
            started_at: system_clock().now(),
            clock_cost: clock_cost(),
            clock_resolution: clock_resolution(),
            store_dir: dir.map_or_else(std::env::temp_dir, Path::to_path_buf),
        }
    }
}
