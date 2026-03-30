//! [`Grid`]: a belay-common [`Table`] that can also color its header and
//! chosen columns.
//!
//! `Table` replaces control characters in cells (they would break the
//! layout), so escape sequences cannot go into cells. Instead the table is
//! rendered plain and colors are spliced in afterwards at the column
//! offsets, which are recomputed here with the same rules `Table` uses:
//! widths in characters, two spaces between columns, cells padded on the
//! right unless the column is right-aligned.

use belay_common::text::{truncate_middle, Align, Table};

use super::style::Style;

/// Chooses a style for a cell from its text.
pub(crate) type Painter = fn(&str) -> Option<Style>;

/// Rows under headers, with per-column alignment, width limits and
/// coloring.
#[derive(Clone, Debug)]
pub(crate) struct Grid {
    headers: Vec<&'static str>,
    rows: Vec<Vec<String>>,
    align: Vec<Align>,
    max_width: Vec<Option<usize>>,
    painters: Vec<Option<Painter>>,
}

impl Grid {
    pub(crate) fn new(headers: &[&'static str]) -> Grid {
        let n = headers.len();
        Grid {
            headers: headers.to_vec(),
            rows: Vec::new(),
            align: vec![Align::Left; n],
            max_width: vec![None; n],
            painters: vec![None; n],
        }
    }

    /// Right-aligns the column with this header.
    pub(crate) fn right(mut self, header: &str) -> Grid {
        if let Some(i) = self.column(header) {
            self.align[i] = Align::Right;
        }
        self
    }

    /// Limits the column with this header to `width` characters.
    pub(crate) fn max_width(mut self, header: &str, width: usize) -> Grid {
        if let Some(i) = self.column(header) {
            self.max_width[i] = Some(width);
        }
        self
    }

    /// Colors the cells of the column with this header.
    pub(crate) fn paint(mut self, header: &str, painter: Painter) -> Grid {
        if let Some(i) = self.column(header) {
            self.painters[i] = Some(painter);
        }
        self
    }

    fn column(&self, header: &str) -> Option<usize> {
        self.headers.iter().position(|h| *h == header)
    }

    pub(crate) fn push<I, S>(&mut self, cells: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut row: Vec<String> = cells.into_iter().map(Into::into).collect();
        row.resize(self.headers.len(), String::new());
        self.rows.push(row);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rendered table; with `color`, the header is bold and painted
    /// columns are colored.
    pub(crate) fn render(&self, color: bool) -> String {
        let mut table = Table::new(&self.headers);
        for (i, align) in self.align.iter().enumerate() {
            table = table.align(i, *align);
        }
        for (i, width) in self.max_width.iter().enumerate() {
            if let Some(width) = width {
                table = table.max_width(i, *width);
            }
        }
        for row in &self.rows {
            table.push_row(row);
        }
        let plain = table.to_string();
        if !color {
            return plain;
        }
        let widths = self.widths();
        let mut out = String::with_capacity(plain.len() + 64);
        for (n, line) in plain.lines().enumerate() {
            if n == 0 {
                out.push_str(&Style::Bold.paint(line));
            } else {
                out.push_str(&self.paint_line(line, &self.rows[n - 1], &widths));
            }
            out.push('\n');
        }
        out
    }

    /// Column widths exactly as `Table` computes them.
    fn widths(&self) -> Vec<usize> {
        (0..self.headers.len())
            .map(|i| {
                let cell_width = |text: &str| match self.max_width[i] {
                    Some(max) => truncate_middle(text, max).chars().count(),
                    None => text.chars().count(),
                };
                self.rows
                    .iter()
                    .map(|r| cell_width(&clean(&r[i])))
                    .chain([cell_width(self.headers[i])])
                    .max()
                    .unwrap_or(0)
            })
            .collect()
    }

    fn paint_line(&self, line: &str, row: &[String], widths: &[usize]) -> String {
        let chars: Vec<char> = line.chars().collect();
        // (start, len, style) of every painted cell, left to right.
        let mut spans = Vec::new();
        let mut start = 0;
        for (i, width) in widths.iter().enumerate() {
            if let Some(style) = self.painters[i].and_then(|p| p(&row[i])) {
                let text = clean(&row[i]);
                let len = match self.max_width[i] {
                    Some(max) => truncate_middle(&text, max).chars().count(),
                    None => text.chars().count(),
                };
                let at = match self.align[i] {
                    Align::Left => start,
                    Align::Right => start + width - len,
                };
                if len > 0 && at + len <= chars.len() {
                    spans.push((at, len, style));
                }
            }
            start += width + 2;
        }
        let mut out = String::with_capacity(line.len() + spans.len() * 10);
        let mut pos = 0;
        for (at, len, style) in spans {
            out.extend(&chars[pos..at]);
            let cell: String = chars[at..at + len].iter().collect();
            out.push_str(&style.paint(&cell));
            pos = at + len;
        }
        out.extend(&chars[pos..]);
        out
    }
}

/// What `Table` does to cells: control characters become spaces.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::style::state_style;

    fn grid() -> Grid {
        let mut g = Grid::new(&["ID", "STATE", "N"])
            .right("N")
            .paint("STATE", state_style)
            .paint("N", |n| (n != "0").then_some(Style::Red));
        g.push(["a", "dead", "12"]);
        g.push(["bbbb", "ready", "0"]);
        g.push(["c", "odd", "3"]);
        g
    }

    #[test]
    fn plain_rendering_is_the_common_table() {
        assert_eq!(
            grid().render(false),
            "ID    STATE   N\na     dead   12\nbbbb  ready   0\nc     odd     3\n"
        );
    }

    #[test]
    fn colored_rendering_only_adds_escape_sequences() {
        let colored = grid().render(true);
        let stripped = strip(&colored);
        assert_eq!(stripped, grid().render(false));
        let lines: Vec<&str> = colored.lines().collect();
        assert_eq!(lines[0], "\x1b[1mID    STATE   N\x1b[0m");
        assert_eq!(lines[1], "a     \x1b[31mdead\x1b[0m   \x1b[31m12\x1b[0m");
        assert_eq!(lines[2], "bbbb  \x1b[34mready\x1b[0m   0");
        assert_eq!(lines[3], "c     odd     \x1b[31m3\x1b[0m");
    }

    #[test]
    fn truncated_cells_are_painted_at_their_shortened_width() {
        let mut g = Grid::new(&["ID", "STATE"])
            .max_width("ID", 5)
            .paint("STATE", state_style);
        g.push(["0123456789", "succeeded"]);
        let colored = g.render(true);
        assert_eq!(strip(&colored), g.render(false));
        assert!(
            colored.contains("01…89  \x1b[32msucceeded\x1b[0m"),
            "{colored}"
        );
    }

    #[test]
    fn short_rows_are_padded() {
        let mut g = Grid::new(&["A", "B"]);
        g.push(["x"]);
        assert!(!g.is_empty());
        assert_eq!(g.render(false), "A  B\nx\n");
    }

    fn strip(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
