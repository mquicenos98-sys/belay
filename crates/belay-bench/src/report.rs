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

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// The columns shown for a scenario: MB/S only if some case counts
/// bytes, latency only if some case records it, notes only if any.
fn scenario_table(rows: &[&CaseResult]) -> Table {
    let with_bytes = rows.iter().any(|r| r.mb_per_sec().is_some());
    let with_latency = rows.iter().any(|r| r.latency().is_some());
    let with_notes = rows.iter().any(|r| !r.notes.is_empty());

    let mut headers = vec!["CASE", "OPS", "TIME", "RATE"];
    if with_bytes {
        headers.push("MB/S");
    }
    if with_latency {
        headers.extend(["P50", "P90", "P99", "P99.9", "MAX"]);
    }
    if with_notes {
        headers.push("NOTES");
    }
    let numeric = headers.len() - usize::from(with_notes);
    let mut table = Table::new(&headers);
    for column in 1..numeric {
        table = table.align(column, Align::Right);
    }
    for r in rows {
        let mut cells = vec![
            r.params.to_string(),
            format_count(r.sample.items),
            format_elapsed(r.sample.elapsed),
            format_rate(r.rate()),
        ];
        if with_bytes {
            cells.push(format_mb_per_sec(r.sample.bytes, r.sample.elapsed));
        }
        if with_latency {
            match r.latency() {
                Some(l) => cells.extend([l.p50, l.p90, l.p99, l.p999, l.max].map(format_nanos)),
                None => cells.extend(["-"; 5].map(String::from)),
            }
        }
        if with_notes {
            cells.push(r.notes_text());
        }
        table.push_row(cells);
    }
    table
}

fn result_json(r: &CaseResult) -> Value {
    let params = r
        .params
        .iter()
        .fold(ObjectBuilder::new(), |b, (k, v)| b.field(k, v))
        .build();
    let notes = r
        .notes
        .iter()
        .fold(ObjectBuilder::new(), |b, (k, v)| b.field(k, v.as_str()))
        .build();
    ObjectBuilder::new()
        .field("scenario", r.scenario)
        .field("case", r.params.to_string())
        .raw("params", params)
        .field("iterations", r.sample.iterations)
        .field("items", r.sample.items)
        .field("bytes", r.sample.bytes)
        .field("elapsed_ns", nanos(r.sample.elapsed))
        .field("items_per_sec", r.rate())
        .field("mb_per_sec", r.mb_per_sec())
        .raw("latency_ns", r.latency().map_or(Value::Null, summary_json))
        .raw("notes", notes)
        .build()
}

fn summary_json(s: Summary) -> Value {
    ObjectBuilder::new()
        .field("count", s.count)
        .field("min", s.min)
        .field("mean", s.mean)
        .field("p50", s.p50)
        .field("p90", s.p90)
        .field("p99", s.p99)
        .field("p999", s.p999)
        .field("max", s.max)
        .build()
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render_tables())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{Params, Sample, Work};

    fn report() -> Report {
        let mut timed = Sample::new();
        for us in 1..=100 {
            timed.record(Duration::from_micros(us), Work::items(16).with_bytes(4096));
        }
        timed.elapsed = Duration::from_millis(10);
        let mut drained = Sample::new();
        drained.add(Work::items(5_000));
        drained.elapsed = Duration::from_secs(2);
        Report {
            environment: Environment::detect(None),
            settings: Settings::new(),
            results: vec![
                CaseResult::new(
                    "store-write",
                    Params::new().with("sync", "never").with("batch", 16),
                    timed,
                )
                .note("syncs/write", "0.00"),
                CaseResult::new("worker-e2e", Params::new().with("mode", "drain"), drained),
            ],
            failures: vec![("cron", Error::Invalid("no fire time found".into()))],
        }
    }

    #[test]
    fn tables_show_only_the_columns_a_scenario_uses() {
        let text = report().render_tables();
        let store = text
            .lines()
            .find(|l| l.starts_with("CASE") && l.contains("MB/S"))
            .expect("store-write has a byte column");
        assert!(
            store.contains("P99.9") && store.ends_with("NOTES"),
            "{store}"
        );
        let row = text.lines().find(|l| l.starts_with("sync=never")).unwrap();
        // 1600 items in 10ms; 409,600 bytes in 10ms.
        assert!(
            row.contains("1.60k") && row.contains("160k/s") && row.contains("41.0"),
            "{row}"
        );
        // The maximum is exact; percentiles round up to their bucket edge.
        assert!(
            row.contains("100us") && row.ends_with("syncs/write=0.00"),
            "{row}"
        );
        let worker = text
            .lines()
            .skip_while(|l| !l.starts_with("worker-e2e:"))
            .nth(1)
            .unwrap();
        assert_eq!(
            worker.split_whitespace().collect::<Vec<_>>(),
            ["CASE", "OPS", "TIME", "RATE"]
        );
    }

    #[test]
    fn json_output_parses_and_carries_every_measurement() {
        let text = belay_json::to_string(&report().to_json());
        let doc = belay_json::parse(&text).unwrap();
        assert_eq!(doc["format"].as_u64(), Some(JSON_FORMAT));
        assert_eq!(
            doc["settings"]["seed"].as_u64(),
            Some(crate::args::DEFAULT_SEED)
        );
        let first = &doc["results"][0];
        assert_eq!(first["case"].as_str(), Some("sync=never batch=16"));
        assert_eq!(first["params"]["batch"].as_str(), Some("16"));
        assert_eq!(first["items"].as_u64(), Some(1_600));
        assert_eq!(first["latency_ns"]["max"].as_u64(), Some(100_000));
        assert_eq!(first["notes"]["syncs/write"].as_str(), Some("0.00"));
        let second = &doc["results"][1];
        assert!(second["latency_ns"].is_null() && second["mb_per_sec"].is_null());
        assert_eq!(second["items_per_sec"].as_f64(), Some(2_500.0));
        assert_eq!(
            doc["failures"][0]["error"].as_str(),
            Some("no fire time found")
        );
    }

    #[test]
    fn header_names_the_build_and_settings() {
        let env = Environment::detect(Some(Path::new("/data/bench")));
        let settings = Settings::new().filter("cron".parse().unwrap());
        let text = header(&env, &settings);
        assert!(text.starts_with(&format!("belay-bench {}", crate::VERSION)));
        assert!(text.contains(&format!("{} cpus", env.cpus)));
        assert!(text.contains("scale full  seed 0xbe1a5eed"), "{text}");
        assert!(text.contains("filters: cron"));
        assert_eq!(env.store_dir, PathBuf::from("/data/bench"));
    }
}
