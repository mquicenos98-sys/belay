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

/// The whole command: parses `args` (without the program name), runs,
/// prints results to `out` and diagnostics to `err`, and returns the exit
/// code: 0 on success, 1 if a scenario failed, 2 for usage errors and
/// filters that select nothing.
pub fn run<I, S>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let settings = match args::parse(args) {
        Ok(Command::Run(settings)) => settings,
        Ok(Command::Help) => return print(out, &args::help_text()),
        Ok(Command::List) => return print(out, &list_text()),
        Err(e) => {
            let _ = writeln!(err, "belay-bench: {e}\nrun `belay-bench --help` for usage");
            return e.exit_code();
        }
    };
    if !settings.verbose {
        // The engine and server log every start, stop and request at info
        // level, which would bury the progress lines.
        set_max_level(LevelFilter::Warn);
    }
    let environment = Environment::detect(settings.dir.as_deref());
    if !settings.json && print(out, &report::header(&environment, &settings)) != 0 {
        return 1;
    }
    let report = execute(environment, settings, Some(&mut *err));
    let rendered = if report.settings.json {
        let mut text = belay_json::to_string_pretty(&report.to_json());
        text.push('\n');
        text
    } else {
        report.render_tables()
    };
    if print(out, &rendered) != 0 {
        return 1;
    }
    for (name, e) in &report.failures {
        let _ = writeln!(err, "belay-bench: {name} failed: {e}");
    }
    if !report.failures.is_empty() {
        1
    } else if report.results.is_empty() {
        let _ = writeln!(err, "belay-bench: no case matched the filters");
        2
    } else {
        0
    }
}

fn print(out: &mut dyn Write, text: &str) -> u8 {
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

fn list_text() -> String {
    scenarios::SCENARIOS
        .iter()
        .map(|s| format!("{:<16} {}\n", s.name, s.summary))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_capture(args: &[&str]) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(args.iter().copied(), &mut out, &mut err);
        let text = |v: Vec<u8>| String::from_utf8(v).expect("output is UTF-8");
        (code, text(out), text(err))
    }

    #[test]
    fn help_and_list_print_to_stdout() {
        let (code, out, _) = run_capture(&["--help"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("belay-bench: throughput"));
        let (code, out, _) = run_capture(&["--list"]);
        assert_eq!(code, 0);
        assert_eq!(out.lines().count(), scenarios::SCENARIOS.len());
    }

    #[test]
    fn usage_errors_exit_with_two() {
        let (code, out, err) = run_capture(&["--bogus"]);
        assert_eq!(code, 2);
        assert!(out.is_empty());
        assert!(
            err.starts_with("belay-bench: unknown option --bogus"),
            "{err}"
        );
    }

    #[test]
    fn filters_that_select_nothing_are_an_error() {
        let (code, out, err) = run_capture(&["cron", "--smoke", "--filter", "tz=Mars/Olympus"]);
        assert_eq!(code, 2);
        assert!(
            out.contains("scale smoke"),
            "the header is still printed: {out}"
        );
        assert!(err.contains("no case matched the filters"), "{err}");
    }

    #[test]
    fn a_small_run_prints_a_table_or_json() {
        let (code, out, err) =
            run_capture(&["cron", "--smoke", "--filter", "expr=every-5m,tz=UTC"]);
        assert_eq!(code, 0, "{err}");
        assert!(
            out.contains("\ncron: ") && out.contains("expr=every-5m tz=UTC"),
            "{out}"
        );
        assert!(err.starts_with("cron "), "one progress line: {err}");

        let (code, out, _) = run_capture(&[
            "cron",
            "--smoke",
            "--json",
            "--filter",
            "tz=UTC,expr=leap-day",
        ]);
        assert_eq!(code, 0);
        let doc = belay_json::parse(&out).expect("stdout is one JSON document");
        assert_eq!(doc["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            doc["results"][0]["params"]["expr"].as_str(),
            Some("leap-day")
        );
    }

    #[test]
    fn unknown_scenarios_in_settings_are_failures() {
        let mut settings = Settings::new().scale(Scale::Smoke);
        settings.scenarios = vec!["nonesuch"];
        let report = execute(Environment::detect(None), settings, None);
        assert_eq!(report.failures.len(), 1);
        assert!(report.results.is_empty());
    }
}
