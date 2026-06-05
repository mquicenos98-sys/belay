//! Command-line parsing: which scenarios to run, how big, which cases,
//! and how to report.
//!
//! ```text
//! belay-bench [SCENARIO...] [--quick | --smoke] [--filter TERMS]... [--json]
//!             [--seed N] [--threads N] [--dir PATH] [--verbose]
//! belay-bench --list | --help
//! ```
//!
//! Options accept `--name value` and `--name=value`; `--` ends them.

use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::FromStr;

use crate::harness::Params;
use crate::scenarios::{self, SCENARIOS};
use crate::{Error, Result};

/// Seed used when `--seed` is not given, so that two runs produce the
/// same keys, values and documents.
pub const DEFAULT_SEED: u64 = 0xBE1A_5EED;

/// Largest accepted `--threads`.
pub const MAX_THREADS: usize = 1024;

/// How much work each scenario does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scale {
    /// The full matrix with iteration counts large enough for stable
    /// percentiles. Takes several minutes.
    Full,
    /// A reduced matrix with fewer iterations, for CI.
    Quick,
    /// One or two iterations of a minimal matrix: checks that every case
    /// runs, measures nothing useful.
    Smoke,
}

impl fmt::Display for Scale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Scale::Full => "full",
            Scale::Quick => "quick",
            Scale::Smoke => "smoke",
        })
    }
}

/// What the command line asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Run benchmarks.
    Run(Settings),
    /// Print the scenarios.
    List,
    /// Print the usage text.
    Help,
}

/// Everything that shapes a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Scenarios to run, in the order given (default: all, in registry
    /// order).
    pub scenarios: Vec<&'static str>,
    /// Size of the run.
    pub scale: Scale,
    /// A case runs if any filter matches it (or there are none).
    pub filters: Vec<Filter>,
    /// Seed for every random number the scenarios use.
    pub seed: u64,
    /// Print JSON instead of tables.
    pub json: bool,
    /// Highest thread, client or worker-slot count the scenarios use.
    pub max_threads: usize,
    /// Directory for store files; the system temp directory if `None`.
    pub dir: Option<PathBuf>,
    /// Keep belay's info-level logs instead of only warnings.
    pub verbose: bool,
}

impl Settings {
    /// Every scenario at full scale, default seed, tables.
    #[must_use]
    pub fn new() -> Settings {
        Settings {
            scenarios: SCENARIOS.iter().map(|s| s.name).collect(),
            scale: Scale::Full,
            filters: Vec::new(),
            seed: DEFAULT_SEED,
            json: false,
            max_threads: default_max_threads(),
            dir: None,
            verbose: false,
        }
    }

    /// Sets the scale.
    #[must_use]
    pub fn scale(mut self, scale: Scale) -> Settings {
        self.scale = scale;
        self
    }

    /// Adds a filter.
    #[must_use]
    pub fn filter(mut self, filter: Filter) -> Settings {
        self.filters.push(filter);
        self
    }

    /// Whether the case `params` of `scenario` passes the filters.
    #[must_use]
    pub fn wants(&self, scenario: &str, params: &Params) -> bool {
        self.filters.is_empty() || self.filters.iter().any(|f| f.matches(scenario, params))
    }
}

impl Default for Settings {
    fn default() -> Settings {
        Settings::new()
    }
}
