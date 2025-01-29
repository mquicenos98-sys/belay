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
