//! The parser proper: walks the tokens once, descending into subcommands
//! and collecting flag and argument values, then checks arity, repeats and
//! conflicts, and fills in environment and default values.

use std::ffi::OsString;

use super::error::{UsageError, UsageErrorKind};
use super::matches::{Entry, Matches, Source};
use super::suggest::suggest;
use super::{Command, Flag};

/// What the command line asks for.
#[derive(Clone, Debug)]
pub enum Parsed {
    /// Run the command in [`Matches::path`].
    Run(Matches),
    /// Show the help of the command at this path.
    Help(Vec<&'static str>),
    /// Show the version (`--version` on the root).
    Version,
}
