//! The result of a successful parse: which command ran and the values of
//! its flags and arguments, with typed accessors.

use super::error::{UsageError, UsageErrorKind};
use super::value::FromArg;

/// Where a value came from, for error messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// A flag on the command line.
    Flag,
    /// A positional argument.
    Arg,
    /// The flag's environment variable.
    Env(&'static str),
    /// The flag's default.
    Default,
}

/// One occurrence of a flag or argument.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) name: &'static str,
    pub(crate) source: Source,
    /// `None` for an occurrence of a switch.
    pub(crate) value: Option<String>,
}

/// Parsed flags and arguments of one command.
///
/// Accessors take the flag's long name or the argument's name; the two
/// share one namespace per command. A name that was not declared simply
/// reads as absent.
#[derive(Clone, Debug, Default)]
pub struct Matches {
    pub(crate) path: Vec<&'static str>,
    pub(crate) entries: Vec<Entry>,
}

impl Matches {
    /// The command that was selected, as names below the root, e.g.
    /// `["job", "list"]`. Aliases are resolved to the real name.
    #[must_use]
    pub fn path(&self) -> &[&'static str] {
        &self.path
    }

    fn occurrences(&self, name: &str) -> std::vec::IntoIter<&Entry> {
        let found: Vec<&Entry> = self.entries.iter().filter(|e| e.name == name).collect();
        found.into_iter()
    }

    /// Whether a switch was given (on the command line; switches have no
    /// environment or default source).
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        self.count(name) > 0
    }

    /// How often a switch was given: `-vv` counts 2.
    #[must_use]
    pub fn count(&self, name: &str) -> usize {
        self.occurrences(name)
            .filter(|e| e.source == Source::Flag && e.value.is_none())
            .count()
    }

    /// Whether the user supplied `name` on the command line (not through
    /// the environment or a default).
    #[must_use]
    pub fn given(&self, name: &str) -> bool {
        self.occurrences(name)
            .any(|e| matches!(e.source, Source::Flag | Source::Arg))
    }

    /// The value of an option or argument: the last one given, else the
    /// environment's, else the default.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&str> {
        self.best(name).and_then(|e| e.value.as_deref())
    }

    /// Every value given for a repeated option or a `many` argument, in
    /// order; falls back to the environment or default like
    /// [`value`](Self::value) when none was given.
    #[must_use]
    pub fn values(&self, name: &str) -> Vec<&str> {
        let given: Vec<&str> = self
            .occurrences(name)
            .filter(|e| matches!(e.source, Source::Flag | Source::Arg))
            .filter_map(|e| e.value.as_deref())
            .collect();
        if given.is_empty() {
            self.value(name).into_iter().collect()
        } else {
            given
        }
    }

    /// A positional argument's value; the same as [`value`](Self::value).
    #[must_use]
    pub fn arg(&self, name: &str) -> Option<&str> {
        self.value(name)
    }

    /// The occurrence that decides the value: command line first (last one
    /// wins), then environment, then default.
    fn best(&self, name: &str) -> Option<&Entry> {
        let mut found: Option<&Entry> = None;
        for entry in self.occurrences(name).filter(|e| e.value.is_some()) {
            let better = match (&found, &entry.source) {
                (None, _) => true,
                (Some(_), Source::Flag | Source::Arg | Source::Env(_)) => true,
                (Some(_), Source::Default) => false,
            };
            if better {
                found = Some(entry);
            }
        }
        found
    }

    /// The value parsed as `T`, or `None` when absent.
    pub fn get<T: FromArg>(&self, name: &str) -> Result<Option<T>, UsageError> {
        self.best(name).map(|e| self.parse_entry(e)).transpose()
    }

    /// The value parsed as `T`, or `default` when absent.
    pub fn get_or<T: FromArg>(&self, name: &str, default: T) -> Result<T, UsageError> {
        Ok(self.get(name)?.unwrap_or(default))
    }

    /// The value parsed as `T`; absent is a usage error naming the flag.
    /// For options the command needs in some modes only (required
    /// arguments are enforced by the parser).
    pub fn require<T: FromArg>(&self, name: &str) -> Result<T, UsageError> {
        self.get(name)?
            .ok_or_else(|| UsageError::constraint(format!("--{name} is required"), &self.path))
    }

    /// Every value parsed as `T` (see [`values`](Self::values)).
    pub fn get_all<T: FromArg>(&self, name: &str) -> Result<Vec<T>, UsageError> {
        let given: Vec<&Entry> = self
            .occurrences(name)
            .filter(|e| matches!(e.source, Source::Flag | Source::Arg) && e.value.is_some())
            .collect();
        if given.is_empty() {
            return Ok(self.get(name)?.into_iter().collect());
        }
        given.into_iter().map(|e| self.parse_entry(e)).collect()
    }

    /// Every value split on commas and parsed as `T`, so that
    /// `--state dead,retrying --state cancelled` gives three states. Empty
    /// items are skipped.
    pub fn get_list<T: FromArg>(&self, name: &str) -> Result<Vec<T>, UsageError> {
        let mut entries: Vec<&Entry> = self
            .occurrences(name)
            .filter(|e| matches!(e.source, Source::Flag | Source::Arg) && e.value.is_some())
            .collect();
        if entries.is_empty() {
            entries.extend(self.best(name));
        }
        let mut out = Vec::new();
        for entry in entries {
            let text = entry.value.as_deref().unwrap_or_default();
            for item in text.split(',').map(str::trim).filter(|i| !i.is_empty()) {
                out.push(T::from_arg(item).map_err(|reason| self.invalid(entry, item, reason))?);
            }
        }
        Ok(out)
    }

    fn parse_entry<T: FromArg>(&self, entry: &Entry) -> Result<T, UsageError> {
        let text = entry.value.as_deref().unwrap_or_default();
        T::from_arg(text).map_err(|reason| self.invalid(entry, text, reason))
    }

    fn invalid(&self, entry: &Entry, value: &str, reason: String) -> UsageError {
        let target = match &entry.source {
            Source::Arg => format!("<{}>", entry.name.to_ascii_uppercase().replace('-', "_")),
            Source::Flag | Source::Default => format!("--{}", entry.name),
            Source::Env(var) => format!("--{} (from {var})", entry.name),
        };
        UsageError::new(
            UsageErrorKind::InvalidValue {
                target,
                value: value.to_owned(),
                reason,
            },
            &self.path,
        )
    }

    /// A usage error about this command, for rules the command checks
    /// itself.
    #[must_use]
    pub fn constraint(&self, message: impl Into<String>) -> UsageError {
        UsageError::constraint(message, &self.path)
    }

    /// Fails with a [`Conflict`](UsageErrorKind::Conflict) when both flags
    /// were given.
    pub fn exclusive(&self, first: &str, second: &str) -> Result<(), UsageError> {
        if self.given(first) && self.given(second) {
            return Err(UsageError::new(
                UsageErrorKind::Conflict {
                    first: format!("--{first}"),
                    second: format!("--{second}"),
                },
                &self.path,
            ));
        }
        Ok(())
    }
}
