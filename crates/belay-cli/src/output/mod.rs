//! Everything commands print goes through [`Output`]: data to stdout (as
//! tables, detail blocks or pretty JSON), notes and warnings to stderr, and
//! colors only when enabled.
//!
//! Conventions: `--json` prints exactly one JSON document per command (one
//! per line for streams); `--quiet` reduces output to identifiers and
//! suppresses confirmations and notes; nothing a script might parse goes
//! to stderr.

pub(crate) mod grid;

pub(crate) mod human;

pub(crate) mod style;

use std::fmt;
use std::io::{self, Write};

use belay_json::{ToJson, Value};

pub(crate) use grid::Grid;
pub(crate) use style::{state_style, Style};

/// The two output streams and the formatting choices of this invocation.
pub(crate) struct Output<'a> {
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    /// `--json`: machine-readable output.
    pub(crate) json: bool,
    /// `--quiet`: identifiers only, no confirmations.
    pub(crate) quiet: bool,
    /// Whether to emit color escape sequences.
    pub(crate) color: bool,
}
