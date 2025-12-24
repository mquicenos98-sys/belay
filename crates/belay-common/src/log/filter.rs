//! Per-target level filtering, configured with directives such as
//! `info,belay_store=debug,belay_http::server=off`.

use std::fmt;
use std::str::FromStr;

use super::{LevelFilter, LogConfigError, Metadata};

/// A default level plus per-target overrides.
///
/// A directive applies to its target and every module below it: the target
/// `belay_store` matches `belay_store` and `belay_store::wal`, but not
/// `belay_storex`. The most specific (longest) matching directive wins.
///
/// ```
/// use belay_common::log::{Filter, LevelFilter};
///
/// let filter: Filter = "warn,belay_store=debug,belay_store::wal=trace".parse().unwrap();
/// assert_eq!(filter.level_for("belay_http"), LevelFilter::Warn);
/// assert_eq!(filter.level_for("belay_store::snapshot"), LevelFilter::Debug);
/// assert_eq!(filter.level_for("belay_store::wal::segment"), LevelFilter::Trace);
/// assert_eq!(filter.max_level(), LevelFilter::Trace);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    default: LevelFilter,
    /// Longest target first, so the first match is the most specific.
    directives: Vec<(String, LevelFilter)>,
}

impl Filter {
    /// A filter with only a default level.
    #[must_use]
    pub fn new(default: impl Into<LevelFilter>) -> Filter {
        Filter {
            default: default.into(),
            directives: Vec::new(),
        }
    }

    /// Adds or replaces the level for `target` and its submodules.
    #[must_use]
    pub fn with_target(mut self, target: &str, level: impl Into<LevelFilter>) -> Filter {
        let level = level.into();
        match self.directives.iter_mut().find(|(t, _)| t == target) {
            Some(entry) => entry.1 = level,
            None => {
                self.directives.push((target.to_owned(), level));
                // Stable sort keeps insertion order among equal lengths,
                // which only matters for Display.
                self.directives.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
            }
        }
        self
    }

    /// Parses comma-separated directives: a bare level sets the default
    /// (`info` when absent) and `target=level` adds an override. Whitespace
    /// around entries and empty entries are ignored.
    pub fn parse(spec: &str) -> Result<Filter, LogConfigError> {
        let mut filter = Filter::default();
        for directive in spec.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            match directive.split_once('=') {
                Some((target, level)) => {
                    let target = target.trim();
                    if target.is_empty() {
                        return Err(LogConfigError::EmptyTarget(directive.to_owned()));
                    }
                    let level: LevelFilter = level.trim().parse()?;
                    filter = filter.with_target(target, level);
                }
                None => filter.default = directive.parse()?,
            }
        }
        Ok(filter)
    }

    /// The level that applies to records from `target`.
    #[must_use]
    pub fn level_for(&self, target: &str) -> LevelFilter {
        self.directives
            .iter()
            .find(|(prefix, _)| covers(prefix, target))
            .map_or(self.default, |(_, level)| *level)
    }

    /// Whether a record with `metadata` passes.
    #[must_use]
    pub fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.level_for(metadata.target()).allows(metadata.level())
    }

    /// The most verbose level any target can reach; set the global maximum
    /// to this so the cheap macro check does not discard records the filter
    /// would accept.
    #[must_use]
    pub fn max_level(&self) -> LevelFilter {
        self.directives
            .iter()
            .map(|(_, level)| *level)
            .fold(self.default, LevelFilter::max)
    }
}

fn covers(prefix: &str, target: &str) -> bool {
    target
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
}

/// `info` with no overrides.
impl Default for Filter {
    fn default() -> Self {
        Filter::new(LevelFilter::Info)
    }
}

impl FromStr for Filter {
    type Err = LogConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Filter::parse(s)
    }
}

/// Renders the directive form accepted by [`Filter::parse`].
impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.default)?;
        for (target, level) in &self.directives {
            write!(f, ",{target}={level}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Level;

    #[test]
    fn default_filter_is_info() {
        let f = Filter::default();
        assert_eq!(f.level_for("anything"), LevelFilter::Info);
        assert!(f.enabled(&Metadata::new(Level::Info, "x")));
        assert!(!f.enabled(&Metadata::new(Level::Debug, "x")));
        assert_eq!(Filter::parse(""), Ok(Filter::default()));
    }

    #[test]
    fn targets_match_on_module_boundaries() {
        let f = Filter::parse("error,belay_store=debug").expect("valid");
        assert_eq!(f.level_for("belay_store"), LevelFilter::Debug);
        assert_eq!(f.level_for("belay_store::wal"), LevelFilter::Debug);
        assert_eq!(f.level_for("belay_storex"), LevelFilter::Error);
        assert_eq!(f.level_for("belay"), LevelFilter::Error);
    }

    #[test]
    fn most_specific_directive_wins_regardless_of_order() {
        let a = Filter::parse("a::b::c=trace,a=warn,a::b=off").expect("valid");
        let b = Filter::parse("a::b=off,a=warn,a::b::c=trace").expect("valid");
        for f in [&a, &b] {
            assert_eq!(f.level_for("a::x"), LevelFilter::Warn);
            assert_eq!(f.level_for("a::b::d"), LevelFilter::Off);
            assert_eq!(f.level_for("a::b::c::e"), LevelFilter::Trace);
        }
    }

    #[test]
    fn later_directives_replace_earlier_ones() {
        let f = Filter::parse("debug, x=info , x=error, warn,").expect("valid");
        assert_eq!(f.level_for("x"), LevelFilter::Error);
        assert_eq!(f.level_for("y"), LevelFilter::Warn);
        assert_eq!(f.to_string(), "warn,x=error");
    }

    #[test]
    fn max_level_covers_every_directive() {
        assert_eq!(Filter::new(LevelFilter::Off).max_level(), LevelFilter::Off);
        let f = Filter::new(Level::Warn).with_target("chatty", Level::Trace);
        assert_eq!(f.max_level(), LevelFilter::Trace);
    }

    #[test]
    fn display_round_trips() {
        let f = Filter::parse("info,belay_http=warn,belay_store::wal=trace").expect("valid");
        assert_eq!(f.to_string().parse::<Filter>(), Ok(f));
    }

    #[test]
    fn rejects_bad_directives() {
        assert_eq!(
            Filter::parse("info,=debug").unwrap_err().to_string(),
            "log filter directive \"=debug\" has an empty target"
        );
        assert_eq!(
            Filter::parse("belay=loud"),
            Err(LogConfigError::UnknownLevel("loud".to_owned()))
        );
        assert!(Filter::parse("verbose").is_err());
    }
}
