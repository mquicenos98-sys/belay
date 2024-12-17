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

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown = echo(&self.input);
        match &self.reason {
            // belay-json already knows how to phrase "did you mean"; reuse it
            // so enum errors read the same in JSON and on the command line.
            Reason::Unknown(known) => {
                let e = FromJsonError::unknown_variant(&shown, self.what, known);
                f.write_str(e.message())
            }
            Reason::Invalid(reason) => {
                write!(f, "invalid {} {}: {reason}", self.what, quote(&shown))
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Shortens long inputs for messages.
fn echo(input: &str) -> String {
    if input.chars().count() <= MAX_ECHOED_CHARS {
        input.to_owned()
    } else {
        let head: String = input.chars().take(MAX_ECHOED_CHARS - 3).collect();
        head + "..."
    }
}

/// Quotes a string as JSON does, so control characters stay visible and the
/// quoting matches belay-json's decoding errors.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    // Writing into a String cannot fail.
    let _ = belay_json::write_string(&mut out, s);
    out
}

/// Anything that can go wrong in this crate.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A text form was rejected.
    Parse(ParseError),
    /// A JSON document failed to parse or did not have the expected shape.
    Json(belay_json::Error),
    /// A value decoded but broke one or more rules.
    Invalid(Violations),
    /// A job state change that the state machine does not allow.
    Transition(TransitionError),
}

/// `Result` defaulting to this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse(e) => e.fmt(f),
            Error::Json(e) => e.fmt(f),
            Error::Invalid(e) => e.fmt(f),
            Error::Transition(e) => e.fmt(f),
        }
    }
}

/// Transparent like `Display`: the source is the wrapped error's source.
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Json(e) => std::error::Error::source(e),
            Error::Parse(_) | Error::Invalid(_) | Error::Transition(_) => None,
        }
    }
}

impl From<ParseError> for Error {
    fn from(e: ParseError) -> Error {
        Error::Parse(e)
    }
}

impl From<belay_json::Error> for Error {
    fn from(e: belay_json::Error) -> Error {
        Error::Json(e)
    }
}

impl From<FromJsonError> for Error {
    fn from(e: FromJsonError) -> Error {
        Error::Json(belay_json::Error::Convert(e))
    }
}

impl From<Violations> for Error {
    fn from(e: Violations) -> Error {
        Error::Invalid(e)
    }
}

impl From<TransitionError> for Error {
    fn from(e: TransitionError) -> Error {
        Error::Transition(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_names_suggest_the_closest_known_one() {
        const STATES: &[&str] = &["ready", "running", "dead"];
        let e = ParseError::unknown("job state", "redy", STATES);
        assert_eq!(
            e.to_string(),
            r#"unknown job state "redy", did you mean "ready"?"#
        );
        let e = ParseError::unknown("job state", "zzz", STATES);
        assert_eq!(
            e.to_string(),
            r#"unknown job state "zzz", expected one of: ready, running, dead"#
        );
        assert_eq!(e.what(), "job state");
        assert_eq!(e.input(), "zzz");
    }

    #[test]
    fn long_inputs_are_shortened_and_control_characters_escaped() {
        let long = "q".repeat(500);
        let e = ParseError::invalid("queue name", &long, "too long");
        let text = e.to_string();
        assert!(text.len() < 120, "{text}");
        assert!(text.contains("qqq...\""));
        // The full input stays available to callers.
        assert_eq!(e.input().len(), 500);
        let e = ParseError::invalid("tag", "a\nb", "bad");
        assert_eq!(e.to_string(), r#"invalid tag "a\nb": bad"#);
    }

    #[test]
    fn crate_error_is_transparent() {
        let e: Error = ParseError::invalid("cursor", "x", "not base32").into();
        assert_eq!(e.to_string(), r#"invalid cursor "x": not base32"#);
        let e: Error = FromJsonError::new("bad").at_key("queue").into();
        assert_eq!(e.to_string(), "$.queue: bad");
    }
}
