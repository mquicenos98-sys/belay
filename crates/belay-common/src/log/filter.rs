//! Per-target level filtering, configured with directives such as
//! `info,belay_store=debug,belay_http::server=off`.

use std::fmt;
use std::str::FromStr;

use super::{LevelFilter, LogConfigError, Metadata};

/// A default level plus per-target overrides.
///
/// A directive applies to its target and every module below it: the target
/// `belay_store` matches `belay_store` and `belay_store::wal`. The most
/// specific (longest) matching directive wins.
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
