//! Lexical layer of the parser: a cursor over the text plus scanners for
//! keys, the four TOML string forms, numbers, booleans and date-times.
//!
//! TOML's lexical grammar depends on context (`true` is a key on the left of
//! `=` and a boolean on the right), so there is no token stream; the
//! structural parser calls the scanner that fits the position it is at.

use crate::source::Span;
use crate::value::Value;

/// A parse failure: where, what, and optionally an earlier related span
/// (such as the first definition of a duplicated key).
#[derive(Debug)]
pub(crate) struct Diag {
    pub(crate) span: Span,
    pub(crate) message: String,
    pub(crate) related: Option<(&'static str, Span)>,
}

impl Diag {
    pub(crate) fn new(span: Span, message: impl Into<String>) -> Diag {
        Diag {
            span,
            message: message.into(),
            related: None,
        }
    }

    pub(crate) fn related(mut self, label: &'static str, span: Option<Span>) -> Diag {
        self.related = span.map(|s| (label, s));
        self
    }
}

pub(crate) type PResult<T> = Result<T, Diag>;

/// One segment of a (possibly dotted) key, with its span in the source.
#[derive(Debug, Clone)]
pub(crate) struct KeySeg {
    pub(crate) name: String,
    pub(crate) span: Span,
}

pub(crate) fn is_bare_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Control characters TOML forbids in comments and strings (tab is allowed).
fn is_forbidden_control(c: char) -> bool {
    (c < ' ' && c != '\t') || c == '\u{7f}'
}
