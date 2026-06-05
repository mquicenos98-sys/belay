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
