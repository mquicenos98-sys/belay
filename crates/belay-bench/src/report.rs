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

/// Times a burst of `Instant::now()` calls. Latencies of sub-microsecond
/// operations include one such call, so the header states its cost.
fn clock_cost() -> Duration {
    const CALLS: u32 = 20_000;
    let started = Instant::now();
    for _ in 0..CALLS {
        black_box(Instant::now());
    }
    started.elapsed() / CALLS
}

/// The smallest nonzero difference between successive `Instant` readings
/// over a few samples, giving up after 50ms on a very coarse clock.
fn clock_resolution() -> Duration {
    let started = Instant::now();
    let mut best = Duration::MAX;
    for _ in 0..100 {
        let first = Instant::now();
        let mut next = Instant::now();
        while next == first {
            next = Instant::now();
        }
        best = best.min(next - first);
        if started.elapsed() > Duration::from_millis(50) {
            break;
        }
    }
    best
}

/// The header printed before a text run.
#[must_use]
pub fn header(env: &Environment, settings: &Settings) -> String {
    let mut text = format!(
        "belay-bench {}  {} build  {}/{}  {} cpus\n\
         started {}  scale {}  seed {:#x}  threads up to {}\n\
         Instant::now() costs {} and ticks every {}; store files under {}\n",
        env.version,
        env.profile,
        env.os,
        env.arch,
        env.cpus,
        env.started_at,
        settings.scale,
        settings.seed,
        settings.max_threads,
        format_elapsed(env.clock_cost),
        format_elapsed(env.clock_resolution),
        env.store_dir.display(),
    );
    if !settings.filters.is_empty() {
        let filters: Vec<String> = settings.filters.iter().map(ToString::to_string).collect();
        text.push_str(&format!("filters: {}\n", filters.join(" | ")));
    }
    if env.profile == "debug" {
        text.push_str("warning: debug build; numbers are not representative (use --release)\n");
    }
    text
}

/// A finished run.
#[derive(Debug)]
pub struct Report {
    /// Where it ran.
    pub environment: Environment,
    /// How it was configured.
    pub settings: Settings,
    /// One entry per case run.
    pub results: Vec<CaseResult>,
    /// Scenarios that stopped with an error, after recording whatever
    /// cases they finished.
    pub failures: Vec<(&'static str, Error)>,
}

impl Report {
    /// One table per scenario, each under a title line.
    #[must_use]
    pub fn render_tables(&self) -> String {
        let mut out = String::new();
        let mut names: Vec<&str> = Vec::new();
        for r in &self.results {
            if !names.contains(&r.scenario) {
                names.push(r.scenario);
            }
        }
        for name in names {
            let rows: Vec<&CaseResult> =
                self.results.iter().filter(|r| r.scenario == name).collect();
            let summary = scenarios::find(name).map_or("", |s| s.summary);
            out.push_str(&format!("\n{name}: {summary}\n"));
            out.push_str(&scenario_table(&rows).to_string());
        }
        out
    }

    /// The whole run as one JSON document.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let env = &self.environment;
        let environment = ObjectBuilder::new()
            .field("os", env.os)
            .field("arch", env.arch)
            .field("cpus", env.cpus as u64)
            .field("profile", env.profile)
            .field("version", env.version)
            .field("started_at", env.started_at.to_rfc3339())
            .field("clock_cost_ns", nanos(env.clock_cost))
            .field("clock_resolution_ns", nanos(env.clock_resolution))
            .field("store_dir", env.store_dir.display().to_string())
            .build();
        let s = &self.settings;
        let settings = ObjectBuilder::new()
            .field("scenarios", s.scenarios.clone())
            .field("scale", s.scale.to_string())
            .field("seed", s.seed)
            .field("max_threads", s.max_threads as u64)
            .field(
                "filters",
                s.filters
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            )
            .build();
        let failures: Vec<Value> = self
            .failures
            .iter()
            .map(|(name, e)| {
                ObjectBuilder::new()
                    .field("scenario", *name)
                    .field("error", e.to_string())
                    .build()
            })
            .collect();
        ObjectBuilder::new()
            .field("format", JSON_FORMAT)
            .raw("environment", environment)
            .raw("settings", settings)
            .field(
                "results",
                self.results.iter().map(result_json).collect::<Vec<_>>(),
            )
            .field("failures", failures)
            .build()
    }
}
