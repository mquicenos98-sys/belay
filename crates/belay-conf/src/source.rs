//! Byte spans over configuration text and their translation into the
//! line/column [`Snippet`]s that diagnostics display.

use crate::error::Snippet;

/// A half-open byte range `[start, end)` into the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl Span {
    pub(crate) fn new(start: usize, end: usize) -> Span {
        Span { start, end }
    }
}

/// Where a value in a [`Document`](crate::Document) came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Loc {
    /// Parsed text. `key` is the span of the key that names the value (absent
    /// for array elements); `value` covers the value itself, or the whole
    /// header line for tables defined with `[header]`.
    Source { key: Option<Span>, value: Span },
    /// Set by an environment variable of this name.
    Env(String),
}

/// Byte offsets of every line start, so that offset-to-line is a binary
/// search instead of a rescan of the text for every diagnostic.
#[derive(Debug, Clone)]
pub(crate) struct LineIndex {
    starts: Vec<usize>,
}
