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
