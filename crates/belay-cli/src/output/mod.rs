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

impl<'a> Output<'a> {
    pub(crate) fn new(out: &'a mut dyn Write, err: &'a mut dyn Write) -> Output<'a> {
        Output {
            out,
            err,
            json: false,
            quiet: false,
            color: false,
        }
    }

    /// A line of data on stdout, printed whatever the mode.
    pub(crate) fn line(&mut self, text: impl fmt::Display) -> io::Result<()> {
        writeln!(self.out, "{text}")
    }

    /// Text on stdout as is (it carries its own newlines).
    pub(crate) fn text(&mut self, text: &str) -> io::Result<()> {
        self.out.write_all(text.as_bytes())
    }

    /// A confirmation on stdout ("paused queue emails"), dropped by
    /// `--quiet` and `--json`.
    pub(crate) fn done(&mut self, text: impl fmt::Display) -> io::Result<()> {
        if self.quiet || self.json {
            return Ok(());
        }
        writeln!(self.out, "{text}")
    }

    /// A remark on stderr ("more results: --cursor ..."), dropped by
    /// `--quiet`.
    pub(crate) fn note(&mut self, text: impl fmt::Display) -> io::Result<()> {
        if self.quiet {
            return Ok(());
        }
        let text = text.to_string();
        let shown = self.paint(&text, Style::Dim);
        writeln!(self.err, "{shown}")
    }

    /// A warning on stderr, always shown.
    pub(crate) fn warn(&mut self, text: impl fmt::Display) -> io::Result<()> {
        let label = self.paint("warning:", Style::Yellow);
        writeln!(self.err, "{label} {text}")
    }

    /// Raw text on stderr, e.g. a multi-line diagnostic that has its own
    /// prefixes.
    pub(crate) fn err_text(&mut self, text: &str) -> io::Result<()> {
        self.err.write_all(text.as_bytes())
    }

    /// `error: ...` and an optional `hint: ...` on stderr.
    pub(crate) fn error(&mut self, message: &str, hint: Option<&str>) -> io::Result<()> {
        let label = self.paint("error:", Style::Red);
        writeln!(self.err, "{label} {message}")?;
        if let Some(hint) = hint {
            let label = self.paint("hint:", Style::Cyan);
            writeln!(self.err, "{label} {hint}")?;
        }
        Ok(())
    }

    /// A value as pretty JSON on stdout.
    pub(crate) fn json<T: ToJson + ?Sized>(&mut self, value: &T) -> io::Result<()> {
        self.json_value(&value.to_json())
    }

    /// A JSON value, pretty, on stdout.
    pub(crate) fn json_value(&mut self, value: &Value) -> io::Result<()> {
        writeln!(self.out, "{}", belay_json::to_string_pretty(value))
    }

    /// A JSON value on one line, for streams (`events tail --json`).
    pub(crate) fn json_line(&mut self, value: &Value) -> io::Result<()> {
        writeln!(self.out, "{}", belay_json::to_string(value))?;
        self.out.flush()
    }

    /// A table on stdout; an empty one prints `empty` as a note instead,
    /// so scripts reading stdout see nothing.
    pub(crate) fn grid(&mut self, grid: &Grid, empty: &str) -> io::Result<()> {
        if grid.is_empty() {
            return self.note(empty);
        }
        let text = grid.render(self.color);
        self.out.write_all(text.as_bytes())
    }

    /// A detail block on stdout.
    pub(crate) fn fields(&mut self, fields: &Fields) -> io::Result<()> {
        let text = fields.render(self.color);
        self.out.write_all(text.as_bytes())
    }

    /// `text` in `style` when colors are on.
    pub(crate) fn paint(&self, text: &str, style: Style) -> String {
        if self.color {
            style.paint(text)
        } else {
            text.to_owned()
        }
    }

    /// A state word in its color.
    pub(crate) fn state(&self, state: &str) -> String {
        match state_style(state) {
            Some(style) => self.paint(state, style),
            None => state.to_owned(),
        }
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.out.flush()?;
        self.err.flush()
    }
}

impl fmt::Debug for Output<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output")
            .field("json", &self.json)
            .field("quiet", &self.quiet)
            .field("color", &self.color)
            .finish_non_exhaustive()
    }
}

/// A block of `label  value` lines, the labels padded to one width.
/// Multi-line values continue under the value column.
#[derive(Clone, Debug, Default)]
pub(crate) struct Fields {
    rows: Vec<(String, String, Option<Style>)>,
}
