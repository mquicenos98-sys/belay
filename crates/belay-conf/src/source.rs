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

impl LineIndex {
    pub(crate) fn new(src: &str) -> LineIndex {
        let mut starts = vec![0];
        starts.extend(
            src.bytes()
                .enumerate()
                .filter(|&(_, b)| b == b'\n')
                .map(|(i, _)| i + 1),
        );
        LineIndex { starts }
    }

    /// 1-based line containing byte `offset`.
    pub(crate) fn line_of(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    }

    pub(crate) fn snippet(&self, src: &str, span: Span, file: Option<&str>) -> Snippet {
        let mut start = span.start.min(src.len());
        // An error at end of input after a final newline would otherwise point
        // at an empty phantom line; show the end of the last real line instead.
        if start == src.len() && src.ends_with('\n') {
            start -= 1;
        }
        let line = self.line_of(start);
        let line_start = self.starts[line - 1];
        let line_end = src[line_start..]
            .find('\n')
            .map_or(src.len(), |i| line_start + i);
        let raw = &src[line_start..line_end];
        let text = raw.strip_suffix('\r').unwrap_or(raw);
        let text_end = line_start + text.len();
        let at = start.min(text_end);
        let width = src[at..span.end.clamp(at, text_end)].chars().count();
        Snippet {
            file: file.map(str::to_owned),
            line,
            column: src[line_start..at].chars().count() + 1,
            text: text.to_owned(),
            width: width.max(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locates_lines_and_character_columns() {
        let src = "a = 1\nname = \"héllo\" x\n";
        let idx = LineIndex::new(src);
        let x = src.find('x').unwrap();
        let s = idx.snippet(src, Span::new(x, x + 1), None);
        assert_eq!((s.line, s.column, s.width), (2, 16, 1));
        assert_eq!(s.text, "name = \"héllo\" x");
    }

    #[test]
    fn end_of_input_points_at_end_of_last_line() {
        let src = "a = 1\r\nb =\r\n";
        let s = LineIndex::new(src).snippet(src, Span::new(src.len(), src.len()), None);
        assert_eq!((s.line, s.column, s.text.as_str()), (2, 4, "b ="));
    }

    #[test]
    fn multi_line_spans_are_clipped_to_the_first_line() {
        let src = "k = \"\"\"abc\ndef\"\"\"";
        let s = LineIndex::new(src).snippet(src, Span::new(4, src.len()), Some("c.toml"));
        assert_eq!((s.line, s.column, s.width), (1, 5, 6));
        assert_eq!(s.file.as_deref(), Some("c.toml"));
    }
}
