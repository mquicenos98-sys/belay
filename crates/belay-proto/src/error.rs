//! Error types: rejected text forms ([`ParseError`]) and the crate-wide
//! [`Error`] that also covers validation, JSON decoding and illegal state
//! transitions.

use std::fmt;

use belay_json::FromJsonError;

use crate::job::TransitionError;
use crate::validate::Violations;

/// Inputs longer than this are shortened in error messages, so a 10 MB
/// "queue name" does not produce a 10 MB log line.
const MAX_ECHOED_CHARS: usize = 64;

/// A text form was rejected: an identifier, an enum name, a date, a cursor,
/// an event pattern.
///
/// `Display` names what was being parsed and echoes the input:
/// `invalid queue name "Emails": must start with a lowercase letter or
/// digit, got 'E'`, or for enums `unknown job state "redy", did you mean
/// "ready"?`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    what: &'static str,
    input: String,
    reason: Reason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Reason {
    /// Not one of a fixed set of names.
    Unknown(&'static [&'static str]),
    /// Anything else, already phrased.
    Invalid(String),
}

impl ParseError {
    /// `input` is not a valid `what` for the given reason.
    pub(crate) fn invalid(
        what: &'static str,
        input: &str,
        reason: impl fmt::Display,
    ) -> ParseError {
        ParseError {
            what,
            input: input.to_owned(),
            reason: Reason::Invalid(reason.to_string()),
        }
    }

    /// `input` is not one of the `known` names of `what`.
    pub(crate) fn unknown(
        what: &'static str,
        input: &str,
        known: &'static [&'static str],
    ) -> ParseError {
        ParseError {
            what,
            input: input.to_owned(),
            reason: Reason::Unknown(known),
        }
    }

    /// What was being parsed, e.g. `"queue name"`.
    #[must_use]
    pub fn what(&self) -> &'static str {
        self.what
    }

    /// The rejected input.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}
