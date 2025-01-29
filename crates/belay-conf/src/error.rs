//! Errors and warnings. Anything that can point at configuration text is
//! rendered like a compiler diagnostic: a headline, the location, the
//! offending source line and a caret underline.

use std::fmt::{self, Write as _};
use std::io;
use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// One located line of configuration text, as shown under a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// Name of the file (or other source) when the document was given one.
    pub file: Option<String>,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column, counted in characters.
    pub column: usize,
    /// The complete source line, without its line terminator.
    pub text: String,
    /// Number of characters to underline; at least 1.
    pub width: usize,
}

/// Where a value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The parsed document text.
    Source(Snippet),
    /// An environment variable applied by an [`Overlay`](crate::Overlay).
    Env(String),
}

/// A key present in the document that the consumer never read, usually a
/// typo. Produced by [`Reader::unused_keys`](crate::Reader::unused_keys);
/// `Display` renders it as a warning diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusedKey {
    /// Path of the unused key, e.g. `sever` or `queues.emails.max_concurency`.
    pub path: String,
    /// Where the key was written.
    pub origin: Option<Origin>,
    /// Full path of a key the consumer did ask for that looks like a typo
    /// fix, e.g. `server` for `sever`.
    pub suggestion: Option<String>,
}

impl UnusedKey {
    fn render(&self, f: &mut fmt::Formatter<'_>, severity: &str) -> fmt::Result {
        let help = self
            .suggestion
            .as_ref()
            .map(|s| ("help", format!("did you mean `{s}`?")));
        let message = format!("unknown key `{}`", self.path);
        render(f, severity, &message, self.origin.as_ref(), help)
    }
}

impl fmt::Display for UnusedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.render(f, "warning")
    }
}

/// Everything that can go wrong loading configuration.
///
/// `Display` renders a multi-line diagnostic with the source excerpt when a
/// location is known; [`Error::message`] gives just the one-line headline.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The text is not valid TOML (within the subset this crate accepts).
    /// Covers syntax errors as well as duplicate keys and redefined tables.
    Parse {
        /// What is wrong, e.g. "duplicate key `listen` in table \[server\]".
        message: String,
        /// Where it is wrong.
        at: Snippet,
        /// Extra context, e.g. "first defined on line 3".
        note: Option<String>,
    },
    /// A required key is absent.
    Missing {
        /// Full path of the missing key.
        path: String,
        /// The table that should have contained it, when it exists.
        table: Option<Origin>,
    },
    /// A value has the wrong type.
    Type {
        /// Full path of the value.
        path: String,
        /// Type the consumer asked for.
        expected: &'static str,
        /// Type actually present.
        found: &'static str,
        /// Where the value was set.
        origin: Option<Origin>,
    },
    /// A value has an acceptable type but is not acceptable: out of range,
    /// rejected by a `FromStr` implementation or by consumer validation.
    Invalid {
        /// Full path of the value.
        path: String,
        /// Why the value was rejected.
        message: String,
        /// Where the value was set.
        origin: Option<Origin>,
    },
    /// A key nobody read, reported by
    /// [`Reader::deny_unused`](crate::Reader::deny_unused).
    Unknown(UnusedKey),
    /// A malformed key path was passed to a lookup (a programming error).
    Path {
        /// The path as given.
        path: String,
        /// What is wrong with it.
        message: String,
    },
    /// An environment variable could not be applied as an override.
    Env {
        /// Variable name.
        var: String,
        /// What is wrong with it.
        message: String,
    },
    /// Reading a configuration file failed.
    Io {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
}

impl Error {
    /// The one-line headline, without location or source excerpt.
    pub fn message(&self) -> String {
        match self {
            Error::Parse { message, .. } => message.clone(),
            Error::Missing { path, .. } => format!("missing required key `{path}`"),
            Error::Type {
                path,
                expected,
                found,
                ..
            } => format!(
                "expected {expected} at {}, found {found}",
                display_path(path)
            ),
            Error::Invalid { path, message, .. } => {
                format!("invalid value for {}: {message}", display_path(path))
            }
            Error::Unknown(u) => format!("unknown key `{}`", u.path),
            Error::Path { path, message } => format!("invalid key path `{path}`: {message}"),
            Error::Env { var, message } => {
                format!("cannot apply environment variable {var}: {message}")
            }
            Error::Io { path, source } => format!("cannot read {}: {source}", path.display()),
        }
    }

    /// Where the problem is, when it can be pinned to the document or to an
    /// environment variable.
    pub fn origin(&self) -> Option<Origin> {
        match self {
            Error::Parse { at, .. } => Some(Origin::Source(at.clone())),
            Error::Missing { table, .. } => table.clone(),
            Error::Type { origin, .. } | Error::Invalid { origin, .. } => origin.clone(),
            Error::Unknown(u) => u.origin.clone(),
            Error::Env { var, .. } => Some(Origin::Env(var.clone())),
            Error::Path { .. } | Error::Io { .. } => None,
        }
    }

    /// `(line, column)` of the problem in the document text, if it has one.
    pub fn line_col(&self) -> Option<(usize, usize)> {
        match self.origin() {
            Some(Origin::Source(s)) => Some((s.line, s.column)),
            _ => None,
        }
    }
}

fn display_path(path: &str) -> &str {
    if path.is_empty() {
        "the root table"
    } else {
        path
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unknown(u) => u.render(f, "error"),
            // An env var origin is already named in the headline.
            Error::Env { .. } => render(f, "error", &self.message(), None, None),
            Error::Parse { note, .. } => {
                let note = note.as_ref().map(|n| ("note", n.clone()));
                render(f, "error", &self.message(), self.origin().as_ref(), note)
            }
            _ => render(f, "error", &self.message(), self.origin().as_ref(), None),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Renders `severity: message`, then the location in rustc's layout:
///
/// ```text
/// error: duplicate key `listen` in table [server]
///  --> line 7, column 1
///   |
/// 7 | listen = "0.0.0.0:7700"
///   | ^^^^^^
///   = note: first defined on line 3
/// ```
fn render(
    f: &mut fmt::Formatter<'_>,
    severity: &str,
    message: &str,
    origin: Option<&Origin>,
    footer: Option<(&str, String)>,
) -> fmt::Result {
    write!(f, "{severity}: {message}")?;
    let gutter = match origin {
        Some(Origin::Source(s)) => s.line.to_string().len(),
        _ => 1,
    };
    let pad = " ".repeat(gutter);
    match origin {
        Some(Origin::Source(s)) => {
            match &s.file {
                Some(file) => write!(f, "\n{pad}--> {file}:{}:{}", s.line, s.column)?,
                None => write!(f, "\n{pad}--> line {}, column {}", s.line, s.column)?,
            }
            write!(f, "\n{pad} |\n{:>gutter$} |", s.line)?;
            if !s.text.is_empty() {
                write!(f, " {}", s.text)?;
            }
            write!(f, "\n{pad} | ")?;
            // Reuse the line's own tabs so the caret lines up however the
            // terminal expands them.
            for c in s.text.chars().take(s.column - 1) {
                f.write_char(if c == '\t' { '\t' } else { ' ' })?;
            }
            f.write_str(&"^".repeat(s.width))?;
        }
        Some(Origin::Env(var)) => write!(f, "\n{pad}--> environment variable {var}")?,
        None => {}
    }
    if let Some((label, text)) = footer {
        write!(f, "\n{pad} = {label}: {text}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widens_the_gutter_for_long_line_numbers_and_keeps_tabs() {
        let at = Snippet {
            file: Some("belay.toml".into()),
            line: 120,
            column: 3,
            text: "\tx = 1".into(),
            width: 1,
        };
        let err = Error::Type {
            path: "x".into(),
            expected: "string",
            found: "integer",
            origin: Some(Origin::Source(at)),
        };
        let expected = "error: expected string at x, found integer\n   \
                        --> belay.toml:120:3\n    |\n120 | \tx = 1\n    | \t ^";
        assert_eq!(err.to_string(), expected);
        assert_eq!(err.line_col(), Some((120, 3)));
    }

    #[test]
    fn io_errors_expose_their_source() {
        let err = Error::Io {
            path: PathBuf::from("missing.toml"),
            source: io::Error::new(io::ErrorKind::NotFound, "not found"),
        };
        assert_eq!(
            err.to_string(),
            "error: cannot read missing.toml: not found"
        );
        assert!(std::error::Error::source(&err).is_some());
    }
}
