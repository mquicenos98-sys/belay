//! A benchmark run in progress: the settings every scenario reads, and
//! the results they record.

use std::fmt;
use std::io::Write;
use std::iter;

use belay_common::fs::TempDir;
use belay_common::hash::fnv1a64;
use belay_common::Rng;

use crate::args::{Scale, Settings};
use crate::harness::{Params, Sample};
use crate::histogram::Summary;
use crate::units::{format_nanos, format_rate};
use crate::{Error, Result};

/// The measurements of one case.
#[derive(Clone, Debug, PartialEq)]
pub struct CaseResult {
    /// Scenario name.
    pub scenario: &'static str,
    /// The parameters that identify the case.
    pub params: Params,
    /// What was measured.
    pub sample: Sample,
    /// Scenario-specific observations, such as syncs per write.
    pub notes: Vec<(&'static str, String)>,
}

impl CaseResult {
    /// A result without notes.
    #[must_use]
    pub fn new(scenario: &'static str, params: Params, sample: Sample) -> CaseResult {
        CaseResult {
            scenario,
            params,
            sample,
            notes: Vec::new(),
        }
    }

    /// Adds a note.
    #[must_use]
    pub fn note(mut self, key: &'static str, value: impl fmt::Display) -> CaseResult {
        self.notes.push((key, value.to_string()));
        self
    }

    /// Items per second.
    #[must_use]
    pub fn rate(&self) -> f64 {
        self.sample.rate()
    }

    /// Megabytes (10^6 bytes) per second, if the case counts bytes.
    #[must_use]
    pub fn mb_per_sec(&self) -> Option<f64> {
        let secs = self.sample.elapsed.as_secs_f64();
        (self.sample.bytes > 0 && secs > 0.0).then(|| self.sample.bytes as f64 / 1e6 / secs)
    }

    /// The latency summary, if the case records latencies.
    #[must_use]
    pub fn latency(&self) -> Option<Summary> {
        self.sample.latency.summary()
    }

    /// `key=value` pairs of the notes, space separated.
    #[must_use]
    pub fn notes_text(&self) -> String {
        let pairs: Vec<String> = self.notes.iter().map(|(k, v)| format!("{k}={v}")).collect();
        pairs.join(" ")
    }
}

/// Settings plus collected results, handed to each scenario in turn.
pub struct Session<'a> {
    settings: Settings,
    results: Vec<CaseResult>,
    progress: Option<&'a mut dyn Write>,
}

impl<'a> Session<'a> {
    /// A session that reports nothing while it runs.
    #[must_use]
    pub fn new(settings: Settings) -> Session<'a> {
        Session {
            settings,
            results: Vec::new(),
            progress: None,
        }
    }

    /// Writes a line to `out` as each case finishes.
    #[must_use]
    pub fn with_progress(mut self, out: &'a mut dyn Write) -> Session<'a> {
        self.progress = Some(out);
        self
    }

    /// The settings.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The run's scale.
    #[must_use]
    pub fn scale(&self) -> Scale {
        self.settings.scale
    }

    /// One of three values by scale.
    pub fn pick<T>(&self, full: T, quick: T, smoke: T) -> T {
        match self.settings.scale {
            Scale::Full => full,
            Scale::Quick => quick,
            Scale::Smoke => smoke,
        }
    }

    /// The run's seed.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.settings.seed
    }

    /// A random stream named `stream`, determined by the seed and the
    /// name, so a scenario's data does not change when another scenario
    /// draws more or fewer numbers.
    #[must_use]
    pub fn rng(&self, stream: &str) -> Rng {
        Rng::seed_from_u64(self.settings.seed ^ fnv1a64(stream.as_bytes()))
    }

    /// Whether the filters select this case; scenarios check before any
    /// expensive setup.
    #[must_use]
    pub fn wants(&self, scenario: &str, params: &Params) -> bool {
        self.settings.wants(scenario, params)
    }

    /// Thread counts for scaling cases: powers of two up to
    /// `--threads` (inclusive) at full scale, one and a few otherwise.
    #[must_use]
    pub fn thread_counts(&self) -> Vec<usize> {
        let max = self.settings.max_threads.max(1);
        let mut counts: Vec<usize> = match self.settings.scale {
            Scale::Full => iter::successors(Some(1usize), |n| n.checked_mul(2))
                .take_while(|&n| n < max)
                .chain(iter::once(max))
                .collect(),
            Scale::Quick => vec![1, max.min(4)],
            Scale::Smoke => vec![1, max.min(2)],
        };
        counts.dedup();
        counts
    }

    /// A fresh directory under `--dir` (or the system temp directory),
    /// removed when dropped.
    pub fn temp_dir(&self, prefix: &str) -> Result<TempDir> {
        let dir = match &self.settings.dir {
            Some(parent) => TempDir::new_in(parent, prefix),
            None => TempDir::with_prefix(prefix),
        };
        dir.map_err(|e| Error::io(format!("creating a directory for {prefix}"), e))
    }

    /// Adds a result and reports progress.
    pub fn record(&mut self, result: CaseResult) {
        if let Some(out) = self.progress.as_mut() {
            // Progress is best effort: a closed stderr must not fail the run.
            let _ = writeln!(out, "{}", progress_line(&result));
        }
        self.results.push(result);
    }

    /// The results so far.
    #[must_use]
    pub fn results(&self) -> &[CaseResult] {
        &self.results
    }

    /// The results, consuming the session.
    #[must_use]
    pub fn into_results(self) -> Vec<CaseResult> {
        self.results
    }
}
