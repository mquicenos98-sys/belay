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

/// The available parallelism, capped at 16: beyond that the queue
/// engine's single lock makes more threads a measure of contention, and
/// full runs would take much longer.
#[must_use]
pub fn default_max_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(4, NonZeroUsize::get)
        .clamp(1, 16)
}

/// A `--filter` value: comma-separated terms that must all match.
///
/// A `key=value` term matches a case whose parameter `key` is exactly
/// `value` (so `batch=1` does not match `batch=16`); any other term
/// matches if it occurs in the scenario name or the case label.
///
/// ```
/// use belay_bench::args::Filter;
/// use belay_bench::harness::Params;
///
/// let case = Params::new().with("sync", "never").with("batch", 16);
/// let filter: Filter = "store,batch=16".parse()?;
/// assert!(filter.matches("store-write", &case));
/// assert!(!"batch=1".parse::<Filter>()?.matches("store-write", &case));
/// assert!("never".parse::<Filter>()?.matches("store-write", &case));
/// # Ok::<(), belay_bench::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    text: String,
    terms: Vec<Term>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Term {
    Param { key: String, value: String },
    Text(String),
}

impl Filter {
    /// Whether every term matches the case.
    #[must_use]
    pub fn matches(&self, scenario: &str, params: &Params) -> bool {
        let label = params.to_string();
        self.terms.iter().all(|term| match term {
            Term::Param { key, value } => params.get(key) == Some(value.as_str()),
            Term::Text(text) => scenario.contains(text.as_str()) || label.contains(text.as_str()),
        })
    }
}

impl FromStr for Filter {
    type Err = Error;

    fn from_str(text: &str) -> Result<Filter> {
        let mut terms = Vec::new();
        for raw in text.split(',') {
            let term = raw.trim();
            if term.is_empty() {
                return Err(usage(format!("empty term in filter {text:?}")));
            }
            terms.push(match term.split_once('=') {
                Some((key, value)) if key.is_empty() || value.is_empty() => {
                    return Err(usage(format!(
                        "filter term {term:?} needs both a key and a value"
                    )));
                }
                Some((key, value)) => Term::Param {
                    key: key.to_owned(),
                    value: value.to_owned(),
                },
                None => Term::Text(term.to_owned()),
            });
        }
        Ok(Filter {
            text: text.to_owned(),
            terms,
        })
    }
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Parses the arguments after the program name.
///
/// ```
/// use belay_bench::args::{parse, Command, Scale};
///
/// let Command::Run(settings) = parse(["cron", "--quick", "--seed=7"])? else {
///     unreachable!()
/// };
/// assert_eq!(settings.scenarios, ["cron"]);
/// assert_eq!(settings.scale, Scale::Quick);
/// assert_eq!(settings.seed, 7);
/// # Ok::<(), belay_bench::Error>(())
/// ```
pub fn parse<I, S>(args: I) -> Result<Command>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut settings = Settings::new();
    let mut chosen: Vec<&'static str> = Vec::new();
    let mut scale_flag: Option<String> = None;
    let mut list = false;
    let mut options_done = false;
    let mut args = args.into_iter().map(Into::into);

    while let Some(arg) = args.next() {
        if options_done || !arg.starts_with('-') || arg == "-" {
            for name in scenario_names(&arg)? {
                if !chosen.contains(&name) {
                    chosen.push(name);
                }
            }
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_owned(), Some(value.to_owned()))
            }
            _ => (arg, None),
        };
        let takes_value = matches!(flag.as_str(), "--filter" | "--seed" | "--threads" | "--dir");
        let value = if takes_value {
            match inline.or_else(|| args.next()) {
                Some(value) => value,
                None => return Err(usage(format!("{flag} needs a value"))),
            }
        } else if inline.is_some() {
            return Err(usage(format!("{flag} does not take a value")));
        } else {
            String::new()
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--list" => list = true,
            "--quick" | "--smoke" => {
                if let Some(previous) = scale_flag.as_ref().filter(|p| **p != flag) {
                    return Err(usage(format!("{previous} and {flag} cannot be combined")));
                }
                settings.scale = if flag == "--quick" {
                    Scale::Quick
                } else {
                    Scale::Smoke
                };
                scale_flag = Some(flag);
            }
            "--json" => settings.json = true,
            "-v" | "--verbose" => settings.verbose = true,
            "--filter" => settings.filters.push(value.parse()?),
            "--seed" => settings.seed = parse_seed(&value)?,
            "--threads" => settings.max_threads = parse_threads(&value)?,
            "--dir" => settings.dir = Some(PathBuf::from(value)),
            _ => return Err(usage(format!("unknown option {flag}; see --help"))),
        }
    }
    if list {
        return Ok(Command::List);
    }
    if !chosen.is_empty() {
        settings.scenarios = chosen;
    }
    Ok(Command::Run(settings))
}

fn scenario_names(arg: &str) -> Result<Vec<&'static str>> {
    if arg == "all" {
        return Ok(SCENARIOS.iter().map(|s| s.name).collect());
    }
    match scenarios::find(arg) {
        Some(s) => Ok(vec![s.name]),
        None => Err(usage(format!(
            "unknown scenario {arg:?}; --list shows them all"
        ))),
    }
}

fn parse_seed(value: &str) -> Result<u64> {
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(&hex.replace('_', ""), 16),
        None => value.replace('_', "").parse(),
    };
    parsed.map_err(|_| {
        usage(format!(
            "invalid --seed {value:?}: expected a decimal or 0x-prefixed hex integer"
        ))
    })
}

fn parse_threads(value: &str) -> Result<usize> {
    match value.parse::<usize>() {
        Ok(n) if (1..=MAX_THREADS).contains(&n) => Ok(n),
        _ => Err(usage(format!(
            "invalid --threads {value:?}: expected an integer between 1 and {MAX_THREADS}"
        ))),
    }
}

fn usage(message: String) -> Error {
    Error::Usage(message)
}

/// The `--help` text.
#[must_use]
pub fn help_text() -> String {
    let mut text = String::from(
        "belay-bench: throughput and latency benchmarks for belay's components\n\n\
         usage: belay-bench [SCENARIO...] [OPTIONS]\n\n\
         scenarios (default: all):\n",
    );
    for s in SCENARIOS {
        text.push_str(&format!("  {:<16} {}\n", s.name, s.summary));
    }
    text.push_str(&format!(
        "\noptions:\n\
         \x20 --quick          a reduced matrix with fewer iterations, for CI\n\
         \x20 --smoke          one or two iterations of a minimal matrix; checks that every case runs\n\
         \x20 --filter TERMS   run only cases matching all comma-separated TERMS: key=value matches a\n\
         \x20                  parameter exactly, other terms a substring of the scenario or case;\n\
         \x20                  repeat to run the union\n\
         \x20 --seed N         seed for the random keys, values and documents (default {DEFAULT_SEED:#x})\n\
         \x20 --threads N      most threads, HTTP clients or worker slots a case uses\n\
         \x20                  (default: the CPU count, at most 16; now {})\n\
         \x20 --dir PATH       where the store scenarios create their directories\n\
         \x20                  (default: the system temp directory)\n\
         \x20 --json           print the results as one JSON document on stdout\n\
         \x20 -v, --verbose    keep belay's info-level logs on stderr\n\
         \x20 --list           list the scenarios\n\
         \x20 -h, --help       print this help\n\n\
         Progress goes to stderr, results to stdout. Build with --release: debug builds\n\
         are several times slower and not representative.\n",
        default_max_threads()
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Settings {
        match parse(args.iter().copied()).unwrap() {
            Command::Run(settings) => settings,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    fn error(args: &[&str]) -> String {
        match parse(args.iter().copied()) {
            Err(Error::Usage(message)) => message,
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_runs_everything_at_full_scale() {
        let settings = run(&[]);
        assert_eq!(settings, Settings::new());
        assert_eq!(settings.scenarios.len(), SCENARIOS.len());
        assert_eq!((settings.scale, settings.seed), (Scale::Full, DEFAULT_SEED));
    }

    #[test]
    fn scenarios_keep_their_order_without_duplicates() {
        assert_eq!(run(&["json", "cron", "json"]).scenarios, ["json", "cron"]);
        assert_eq!(run(&["all"]).scenarios, Settings::new().scenarios);
    }

    #[test]
    fn options_take_inline_or_separate_values() {
        let args =
            "--seed 0xff --threads=3 --filter sync=never --filter=cron --dir /mnt/b --json -v";
        let settings = run(&args.split(' ').collect::<Vec<_>>());
        assert_eq!((settings.seed, settings.max_threads), (255, 3));
        let filters: Vec<String> = settings.filters.iter().map(ToString::to_string).collect();
        assert_eq!(filters, ["sync=never", "cron"]);
        assert_eq!(settings.dir, Some(PathBuf::from("/mnt/b")));
        assert!(settings.json && settings.verbose);
        assert_eq!(run(&["--seed", "1_000"]).seed, 1000);
    }

    #[test]
    fn scale_flags_are_exclusive_but_repeatable() {
        assert_eq!(run(&["--quick"]).scale, Scale::Quick);
        assert_eq!(run(&["--smoke", "--smoke"]).scale, Scale::Smoke);
        assert_eq!(
            error(&["--quick", "--smoke"]),
            "--quick and --smoke cannot be combined"
        );
    }

    #[test]
    fn help_and_list_are_commands() {
        assert_eq!(parse(["--quick", "-h"]).unwrap(), Command::Help);
        assert_eq!(parse(["--list"]).unwrap(), Command::List);
        assert!(help_text().contains("store-write"));
    }

    #[test]
    fn mistakes_get_specific_messages() {
        assert_eq!(error(&["--quik"]), "unknown option --quik; see --help");
        assert_eq!(error(&["--seed"]), "--seed needs a value");
        assert_eq!(error(&["--json=yes"]), "--json does not take a value");
        assert_eq!(
            error(&["--threads", "0"]),
            "invalid --threads \"0\": expected an integer between 1 and 1024"
        );
        assert!(error(&["--seed", "0xZZ"]).starts_with("invalid --seed \"0xZZ\""));
        assert_eq!(
            error(&["store"]),
            "unknown scenario \"store\"; --list shows them all"
        );
        // `--` ends the options: what follows is a scenario name.
        assert!(error(&["--", "--quick"]).starts_with("unknown scenario \"--quick\""));
    }

    #[test]
    fn param_terms_match_exactly_and_text_terms_as_substrings() {
        let case = Params::new().with("sync", "always").with("batch", 16);
        let matches = |f: &str| f.parse::<Filter>().unwrap().matches("store-write", &case);
        assert!(matches("batch=16"));
        assert!(
            !matches("batch=1"),
            "a parameter term is not a prefix match"
        );
        assert!(matches("store-write,sync=always"));
        assert!(!matches("store-write,sync=never"), "all terms must match");
        assert!(
            matches("write") && matches("always"),
            "text searches name and label"
        );
        assert!(!matches("queue"));
        assert!(!matches("value=128"), "a missing parameter does not match");
    }

    #[test]
    fn malformed_filters_are_rejected() {
        let message = |f: &str| f.parse::<Filter>().unwrap_err().to_string();
        assert_eq!(message("a,,b"), "empty term in filter \"a,,b\"");
        assert_eq!(
            message("=x"),
            "filter term \"=x\" needs both a key and a value"
        );
        assert_eq!(
            message("batch="),
            "filter term \"batch=\" needs both a key and a value"
        );
    }

    #[test]
    fn any_filter_may_select_a_case() {
        let settings = Settings::new()
            .filter("cron".parse().unwrap())
            .filter("batch=16".parse().unwrap());
        assert!(settings.wants("store-write", &Params::new().with("batch", 16)));
        assert!(settings.wants("cron", &Params::new()));
        assert!(!settings.wants("json", &Params::new()));
        assert!(Settings::new().wants("json", &Params::new()));
    }
}
