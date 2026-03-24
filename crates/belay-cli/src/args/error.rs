//! Command-line mistakes: what went wrong, in which command, and a hint
//! when one can be offered.

use std::fmt;

/// What was wrong with the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UsageErrorKind {
    /// A subcommand name that does not exist.
    UnknownCommand {
        /// The name as typed.
        name: String,
        /// The closest existing name.
        suggestion: Option<String>,
    },
    /// A group was invoked without one of its subcommands.
    MissingCommand {
        /// The names it offers.
        available: Vec<String>,
    },
    /// A flag this command does not know.
    UnknownFlag {
        /// The flag as typed, with dashes.
        flag: String,
        /// The closest known flag, with dashes.
        suggestion: Option<String>,
    },
    /// A flag that needs a value was last on the line or followed by
    /// another flag.
    MissingValue {
        /// `--name`.
        flag: String,
        /// The value placeholder, e.g. `N`.
        value_name: String,
    },
    /// A switch was given a value with `=`.
    UnexpectedValue {
        /// `--name`.
        flag: String,
        /// The value given.
        value: String,
    },
    /// A value that does not parse as what the flag or argument expects.
    InvalidValue {
        /// `--name`, `<NAME>`, or `--name (from VAR)`.
        target: String,
        /// The value given.
        value: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A required positional argument is missing.
    MissingArgument {
        /// `<NAME>`.
        name: String,
    },
    /// More positional arguments than the command takes.
    UnexpectedArgument {
        /// The first extra token.
        value: String,
    },
    /// A flag that may appear once appeared again.
    RepeatedFlag {
        /// `--name`.
        flag: String,
    },
    /// Two flags that exclude each other were both given.
    Conflict {
        /// `--first`.
        first: String,
        /// `--second`.
        second: String,
    },
    /// An argument that is not valid UTF-8.
    NotUtf8 {
        /// The argument, lossily decoded.
        lossy: String,
    },
    /// A rule between flags that the command checks itself, e.g. one of
    /// two flags is required.
    Constraint {
        /// The whole message.
        message: String,
    },
}

/// A mistake on the command line, reported with exit status 2.
///
/// `Display` gives the one-line message; [`hint`](Self::hint) a follow-up
/// line when there is something useful to suggest. The command path lets
/// the caller print the right usage line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageError {
    kind: UsageErrorKind,
    path: Vec<String>,
    hint: Option<String>,
}

impl UsageError {
    /// An error of `kind` in the command at `path` (names below the root).
    #[must_use]
    pub fn new(kind: UsageErrorKind, path: &[&str]) -> UsageError {
        UsageError {
            kind,
            path: path.iter().map(|s| (*s).to_owned()).collect(),
            hint: None,
        }
    }

    /// Replaces the hint [`hint`](Self::hint) would otherwise derive, for
    /// callers that know more about the command than the parser does.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> UsageError {
        self.hint = Some(hint.into());
        self
    }

    /// A [`Constraint`](UsageErrorKind::Constraint) error.
    #[must_use]
    pub fn constraint(message: impl Into<String>, path: &[&str]) -> UsageError {
        UsageError::new(
            UsageErrorKind::Constraint {
                message: message.into(),
            },
            path,
        )
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> &UsageErrorKind {
        &self.kind
    }

    /// The command the error concerns, as names below the root.
    #[must_use]
    pub fn path(&self) -> Vec<&str> {
        self.path.iter().map(String::as_str).collect()
    }

    /// `belay job list`, for messages.
    #[must_use]
    pub fn command_name(&self) -> String {
        std::iter::once("belay")
            .chain(self.path.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A suggestion for fixing the mistake, when there is one.
    #[must_use]
    pub fn hint(&self) -> Option<String> {
        if let Some(hint) = &self.hint {
            return Some(hint.clone());
        }
        match &self.kind {
            UsageErrorKind::UnknownCommand {
                suggestion: Some(s),
                ..
            } => Some(format!("did you mean {s:?}?")),
            UsageErrorKind::UnknownFlag {
                suggestion: Some(s),
                ..
            } => Some(format!("did you mean {s}?")),
            UsageErrorKind::UnknownFlag {
                flag,
                suggestion: None,
            } if flag.starts_with('-') && flag[1..].parse::<f64>().is_ok() => Some(format!(
                "to pass {flag} as a value, put it after `--` or write --flag={flag}"
            )),
            UsageErrorKind::MissingCommand { available } if !available.is_empty() => {
                Some(format!("available commands: {}", available.join(", ")))
            }
            _ => None,
        }
    }
}
