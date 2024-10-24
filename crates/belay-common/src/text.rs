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

/// Formats a byte count with binary (IEC) units and one decimal:
/// `"512 B"`, `"1.5 KiB"`, `"20.0 MiB"`.
#[must_use]
pub fn humanize_bytes(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // 1023.96 KiB would print as "1024.0 KiB"; move up a unit instead.
    if (value * 10.0).round() >= 10_240.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Parses a byte size such as `"512"`, `"64k"`, `"1.5 GiB"` or `"10MB"`.
///
/// Units are case-insensitive. IEC suffixes (`KiB`, `MiB`, `GiB`, `TiB`) are
/// powers of 1024, SI suffixes (`KB`, `MB`, `GB`, `TB`) powers of 1000, and
/// the single letters `K`, `M`, `G`, `T` powers of 1024 as in nginx and JVM
/// flags. One space may separate the number from the unit, so the output of
/// [`humanize_bytes`] parses back (to within its rounding).
///
/// ```
/// use belay_common::text::parse_bytes;
///
/// assert_eq!(parse_bytes("64k"), Ok(65_536));
/// assert_eq!(parse_bytes("1.5 GiB"), Ok(1_610_612_736));
/// assert_eq!(parse_bytes("10MB"), Ok(10_000_000));
/// ```
pub fn parse_bytes(s: &str) -> Result<u64, ParseBytesError> {
    let fail = |kind| ParseBytesError {
        input: s.to_owned(),
        kind,
    };
    let number_end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(number_end);
    let unit = unit.strip_prefix(' ').unwrap_or(unit);

    let (int, frac) = number.split_once('.').unwrap_or((number, ""));
    let digits_ok = |d: &str| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit());
    if !digits_ok(int) || (number.contains('.') && !digits_ok(frac)) {
        return Err(fail(BytesErrorKind::InvalidNumber));
    }

    let multiplier: u128 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        _ => return Err(fail(BytesErrorKind::UnknownUnit(unit.to_owned()))),
    };

    let overflow = || fail(BytesErrorKind::Overflow);
    let whole: u128 = int.parse().map_err(|_| overflow())?;
    // 19 fraction digits are more than enough to resolve single bytes at
    // the largest multiplier (2^40).
    let frac = &frac[..frac.len().min(19)];
    let frac_value: u128 = if frac.is_empty() {
        0
    } else {
        frac.parse().map_err(|_| overflow())?
    };
    let scale = 10u128.pow(frac.len() as u32);
    let total = whole
        .checked_mul(multiplier)
        .and_then(|w| w.checked_add(frac_value * multiplier / scale))
        .ok_or_else(overflow)?;
    u64::try_from(total).map_err(|_| overflow())
}

/// Error returned by [`parse_bytes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseBytesError {
    input: String,
    kind: BytesErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BytesErrorKind {
    InvalidNumber,
    UnknownUnit(String),
    Overflow,
}

impl fmt::Display for ParseBytesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid byte size {:?}: ", self.input)?;
        match &self.kind {
            BytesErrorKind::InvalidNumber => f.write_str("expected a number such as 512 or 1.5"),
            BytesErrorKind::UnknownUnit(unit) => write!(
                f,
                "unknown unit {unit:?} (expected B, K, KB, KiB, M, MB, MiB, G, GB, GiB, T, TB or TiB)"
            ),
            BytesErrorKind::Overflow => f.write_str("value does not fit in 64 bits"),
        }
    }
}

impl std::error::Error for ParseBytesError {}

/// `"1 job"`, `"3 jobs"`, `"2 entries"`, `"0 processes"`.
///
/// Applies the regular English rules (`-s`, `-es` after sibilants, `-ies`
/// after a consonant + `y`); use [`plural_with`] for irregular nouns.
#[must_use]
pub fn plural(count: u64, singular: &str) -> String {
    if count == 1 {
        return format!("1 {singular}");
    }
    let lower = singular.to_ascii_lowercase();
    let ends_with_sibilant = ["s", "x", "z", "ch", "sh"]
        .iter()
        .any(|e| lower.ends_with(e));
    let consonant_y = lower.ends_with('y')
        && lower
            .chars()
            .rev()
            .nth(1)
            .is_some_and(|c| c.is_ascii_alphabetic() && !"aeiou".contains(c));
    if ends_with_sibilant {
        format!("{count} {singular}es")
    } else if consonant_y {
        format!("{count} {}ies", &singular[..singular.len() - 1])
    } else {
        format!("{count} {singular}s")
    }
}

/// Like [`plural`] with an explicit plural form: `plural_with(2, "child",
/// "children")` is `"2 children"`.
#[must_use]
pub fn plural_with(count: u64, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

/// Greedy word wrap to lines of at most `width` characters. Words longer
/// than `width` get a line of their own rather than being split, and runs of
/// whitespace collapse to one space.
#[must_use]
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_len = 0;
    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if line_len > 0 && line_len + 1 + word_len > width {
            lines.push(std::mem::take(&mut line));
            line_len = 0;
        }
        if line_len > 0 {
            line.push(' ');
            line_len += 1;
        }
        line.push_str(word);
        line_len += word_len;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns_and_trims_trailing_space() {
        let mut t = Table::new(["ID", "STATE", "ATTEMPTS"]).align(2, Align::Right);
        t.push_row(["a", "running", "1"]);
        t.push_row(["bbbbbb", "dead", "25"]);
        assert_eq!(
            t.to_string(),
            "ID      STATE    ATTEMPTS\n\
             a       running         1\n\
             bbbbbb  dead           25\n"
        );
        assert_eq!(t.len(), 2);
        assert!(!t.is_empty());
    }

    #[test]
    fn table_pads_short_rows_and_widens_for_long_ones() {
        let mut t = Table::new(["A", "B"]);
        t.push_row(["1"]);
        t.push_row(["1", "2", "3"]);
        assert_eq!(t.to_string(), "A  B\n1\n1  2  3\n");
    }

    #[test]
    fn table_without_headers_prints_only_rows() {
        let mut t = Table::new(Vec::<String>::new());
        t.push_row(["key", "value"]);
        t.push_row(["longer-key", "v"]);
        assert_eq!(t.to_string(), "key         value\nlonger-key  v\n");
    }

    #[test]
    fn empty_table_renders_just_the_header() {
        let t = Table::new(["NAME"]);
        assert!(t.is_empty());
        assert_eq!(t.to_string(), "NAME\n");
        assert_eq!(Table::default().to_string(), "");
    }

    #[test]
    fn table_truncates_cells_over_max_width() {
        let mut t = Table::new(["ID", "N"]).max_width(0, 7);
        t.push_row(["0123456789", "1"]);
        assert_eq!(t.to_string(), "ID       N\n012…789  1\n");
    }

    #[test]
    fn table_replaces_control_characters() {
        let mut t = Table::new(["MSG", "X"]);
        t.push_row(["line1\nline2\tend", "x"]);
        assert_eq!(t.to_string(), "MSG              X\nline1 line2 end  x\n");
    }

    #[test]
    fn table_accepts_display_values() {
        let mut t = Table::new(["N", "OK"]);
        t.push_row([&1 as &dyn fmt::Display, &true]);
        assert_eq!(t.to_string(), "N  OK\n1  true\n");
    }

    #[test]
    fn truncate_middle_respects_char_boundaries() {
        assert_eq!(truncate_middle("héllo wörld", 5), "hé…ld");
        assert_eq!(truncate_middle("abcdef", 6), "abcdef");
        assert_eq!(truncate_middle("abcdef", 5), "ab…ef");
        assert_eq!(truncate_middle("abcdef", 4), "ab…f");
        assert_eq!(truncate_middle("abcdef", 1), "…");
        assert_eq!(truncate_middle("abcdef", 0), "");
        assert!(matches!(truncate_middle("abc", 3), Cow::Borrowed(_)));
    }

    #[test]
    fn humanize_bytes_picks_binary_units() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(1023), "1023 B");
        assert_eq!(humanize_bytes(1024), "1.0 KiB");
        assert_eq!(humanize_bytes(1536), "1.5 KiB");
        assert_eq!(humanize_bytes(20 * 1024 * 1024), "20.0 MiB");
        assert_eq!(humanize_bytes(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn humanize_bytes_promotes_values_that_round_up() {
        // Regression: 1 MiB - 1 used to print as "1024.0 KiB".
        assert_eq!(humanize_bytes(1024 * 1024 - 1), "1.0 MiB");
        assert_eq!(humanize_bytes(1024 * 1024 - 100), "1023.9 KiB");
    }

    #[test]
    fn parse_bytes_accepts_iec_si_and_short_units() {
        assert_eq!(parse_bytes("0"), Ok(0));
        assert_eq!(parse_bytes("512"), Ok(512));
        assert_eq!(parse_bytes("512B"), Ok(512));
        assert_eq!(parse_bytes("64k"), Ok(64 * 1024));
        assert_eq!(parse_bytes("64KiB"), Ok(64 * 1024));
        assert_eq!(parse_bytes("64kb"), Ok(64_000));
        assert_eq!(parse_bytes("16M"), Ok(16 << 20));
        assert_eq!(parse_bytes("2 GB"), Ok(2_000_000_000));
        assert_eq!(parse_bytes("1TiB"), Ok(1 << 40));
        assert_eq!(parse_bytes("0.5k"), Ok(512));
        assert_eq!(parse_bytes("1.5 KiB"), Ok(1536));
    }

    #[test]
    fn parse_bytes_round_trips_humanized_values() {
        for n in [0u64, 1, 1023, 1024, 1536, 5 << 20, 3 << 30] {
            assert_eq!(parse_bytes(&humanize_bytes(n)), Ok(n));
        }
    }

    #[test]
    fn parse_bytes_rejects_bad_input() {
        for bad in ["", "k", ".5k", "1.k", "1..5", "-1", " 1", "1  k", "1e3"] {
            assert!(parse_bytes(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(
            parse_bytes("12XB").unwrap_err().to_string(),
            "invalid byte size \"12XB\": unknown unit \"XB\" (expected B, K, KB, KiB, M, MB, MiB, G, GB, GiB, T, TB or TiB)"
        );
        assert_eq!(
            parse_bytes("16777216TiB").unwrap_err().to_string(),
            "invalid byte size \"16777216TiB\": value does not fit in 64 bits"
        );
        assert_eq!(parse_bytes("16777215TiB"), Ok(16_777_215 << 40));
    }

    #[test]
    fn plural_follows_regular_english_rules() {
        assert_eq!(plural(1, "job"), "1 job");
        assert_eq!(plural(0, "job"), "0 jobs");
        assert_eq!(plural(3, "job"), "3 jobs");
        assert_eq!(plural(2, "entry"), "2 entries");
        assert_eq!(plural(2, "day"), "2 days");
        assert_eq!(plural(2, "process"), "2 processes");
        assert_eq!(plural(2, "batch"), "2 batches");
        assert_eq!(plural(2, "box"), "2 boxes");
        assert_eq!(plural(2, "y"), "2 ys");
        assert_eq!(plural_with(2, "child", "children"), "2 children");
        assert_eq!(plural_with(1, "child", "children"), "1 child");
    }

    #[test]
    fn wrap_breaks_on_word_boundaries() {
        assert_eq!(
            wrap("the quick brown fox jumps over the lazy dog", 10),
            vec!["the quick", "brown fox", "jumps over", "the lazy", "dog"]
        );
        assert_eq!(wrap("  spaced   out  ", 80), vec!["spaced out"]);
        assert_eq!(wrap("a verylongword b", 4), vec!["a", "verylongword", "b"]);
        assert!(wrap("", 10).is_empty());
    }
}
