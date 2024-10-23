//! Plain-text presentation helpers for command-line output.
//!
//! Widths are measured in `char`s. That is exact for the ASCII identifiers,
//! numbers and timestamps belay prints; East Asian wide characters and
//! combining marks will misalign columns.

use std::borrow::Cow;
use std::fmt;

/// Horizontal alignment of a table column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    /// Pad on the right (the default).
    #[default]
    Left,
    /// Pad on the left; the usual choice for numbers.
    Right,
}

/// A table rendered as aligned columns separated by two spaces, in the style
/// of `ps` or `kubectl get`: no borders, one header line, no trailing
/// whitespace.
///
/// ```
/// use belay_common::text::{Align, Table};
///
/// let mut t = Table::new(["QUEUE", "READY", "STATE"]).align(1, Align::Right);
/// t.push_row(["emails", "12", "active"]);
/// t.push_row(["reports", "3", "paused"]);
/// assert_eq!(
///     t.to_string(),
///     "QUEUE    READY  STATE\n\
///      emails      12  active\n\
///      reports      3  paused\n"
/// );
/// ```
#[derive(Clone, Debug, Default)]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    align: Vec<Align>,
    max_width: Vec<Option<usize>>,
}

impl Table {
    /// A table with the given column headers. With no headers, no header
    /// line is printed and the column count follows the widest row.
    pub fn new<I>(headers: I) -> Table
    where
        I: IntoIterator,
        I::Item: fmt::Display,
    {
        Table {
            headers: headers
                .into_iter()
                .map(|h| sanitize(&h.to_string()))
                .collect(),
            ..Table::default()
        }
    }

    /// Sets the alignment of column `column` (0-based).
    #[must_use]
    pub fn align(mut self, column: usize, align: Align) -> Table {
        grow(&mut self.align, column);
        self.align[column] = align;
        self
    }

    /// Limits column `column` to `width` characters; longer cells are
    /// shortened with [`truncate_middle`], which keeps both ends of IDs
    /// recognisable.
    #[must_use]
    pub fn max_width(mut self, column: usize, width: usize) -> Table {
        grow(&mut self.max_width, column);
        self.max_width[column] = Some(width);
        self
    }

    /// Appends a row. Short rows are padded with empty cells.
    pub fn push_row<I>(&mut self, cells: I)
    where
        I: IntoIterator,
        I::Item: fmt::Display,
    {
        self.rows.push(
            cells
                .into_iter()
                .map(|c| sanitize(&c.to_string()))
                .collect(),
        );
    }

    /// Number of data rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the table has no data rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn cell<'a>(&self, row: &'a [String], column: usize) -> Cow<'a, str> {
        let text = row.get(column).map_or("", String::as_str);
        match self.max_width.get(column).copied().flatten() {
            Some(width) => truncate_middle(text, width),
            None => Cow::Borrowed(text),
        }
    }
}

fn grow<T: Default + Clone>(v: &mut Vec<T>, index: usize) {
    if v.len() <= index {
        v.resize(index + 1, T::default());
    }
}

/// Control characters would break the line structure; show them as spaces.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let columns = self
            .rows
            .iter()
            .map(Vec::len)
            .chain([self.headers.len()])
            .max()
            .unwrap_or(0);
        let header_line = (!self.headers.is_empty()).then_some(&self.headers);
        let lines: Vec<&Vec<String>> = header_line.into_iter().chain(&self.rows).collect();

        let mut widths = vec![0usize; columns];
        for line in &lines {
            for (column, width) in widths.iter_mut().enumerate() {
                *width = (*width).max(self.cell(line, column).chars().count());
            }
        }

        let mut out = String::new();
        for line in lines {
            out.clear();
            for (column, &width) in widths.iter().enumerate() {
                if column > 0 {
                    out.push_str("  ");
                }
                let text = self.cell(line, column);
                let pad = width - text.chars().count();
                match self.align.get(column).copied().unwrap_or_default() {
                    Align::Left => {
                        out.push_str(&text);
                        out.extend(std::iter::repeat(' ').take(pad));
                    }
                    Align::Right => {
                        out.extend(std::iter::repeat(' ').take(pad));
                        out.push_str(&text);
                    }
                }
            }
            writeln!(f, "{}", out.trim_end())?;
        }
        Ok(())
    }
}

/// Shortens `s` to at most `max` characters by replacing its middle with
/// `…`. Returns the input unchanged when it already fits.
///
/// ```
/// use belay_common::text::truncate_middle;
///
/// assert_eq!(truncate_middle("01J9ZQ4Y5V3M8TXE2KFBRWH6N7", 11), "01J9Z…WH6N7");
/// assert_eq!(truncate_middle("short", 11), "short");
/// ```
#[must_use]
pub fn truncate_middle(s: &str, max: usize) -> Cow<'_, str> {
    let len = s.chars().count();
    if len <= max {
        return Cow::Borrowed(s);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let keep = max - 1;
    let head = keep - keep / 2;
    let tail = keep / 2;
    let byte_at = |n: usize| s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..byte_at(head)]);
    out.push('…');
    out.push_str(&s[byte_at(len - tail)..]);
    Cow::Owned(out)
}
