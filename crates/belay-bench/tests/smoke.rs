//! Runs every scenario at smoke scale: each case once or twice, so a
//! change that breaks a benchmark fails the test suite rather than the
//! next benchmark run.

use std::collections::BTreeSet;

use belay_bench::scenarios::SCENARIOS;
use belay_bench::{execute, Environment, Scale, Settings};
use belay_common::fs::TempDir;
use belay_common::log::{set_max_level, LevelFilter};

#[test]
fn every_scenario_runs_at_smoke_scale() {
    set_max_level(LevelFilter::Warn);
    let dir = TempDir::with_prefix("belay-bench-smoke").expect("temp dir");
    let mut settings = Settings::new().scale(Scale::Smoke);
    settings.max_threads = 2;
    settings.dir = Some(dir.path().to_path_buf());

    let report = execute(Environment::detect(Some(dir.path())), settings, None);

    let failures: Vec<String> = report
        .failures
        .iter()
        .map(|(name, e)| format!("{name}: {e}"))
        .collect();
    assert!(failures.is_empty(), "scenarios failed: {failures:#?}");

    let ran: BTreeSet<&str> = report.results.iter().map(|r| r.scenario).collect();
    let all: BTreeSet<&str> = SCENARIOS.iter().map(|s| s.name).collect();
    assert_eq!(ran, all, "every scenario records at least one case");

    for r in &report.results {
        assert!(
            r.sample.iterations > 0,
            "{} {} measured nothing",
            r.scenario,
            r.params
        );
        assert!(
            r.sample.items >= r.sample.iterations,
            "{} {}",
            r.scenario,
            r.params
        );
    }
    // Store directories are removed as each case finishes.
    let leftovers = std::fs::read_dir(dir.path()).expect("list dir").count();
    assert_eq!(leftovers, 0, "store cases left directories behind");

    let json = belay_json::to_string(&report.to_json());
    assert!(belay_json::parse(&json).is_ok());
    assert!(!report.render_tables().is_empty());
}
