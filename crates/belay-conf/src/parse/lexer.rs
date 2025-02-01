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

fn describe_char(c: char) -> String {
    if c.is_control() {
        format!("control character U+{:04X}", c as u32)
    } else {
        format!("`{c}`")
    }
}

pub(crate) struct Cursor<'s> {
    src: &'s str,
    pos: usize,
}

impl<'s> Cursor<'s> {
    pub(crate) fn new(src: &'s str) -> Cursor<'s> {
        Cursor { src, pos: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn rest(&self) -> &'s str {
        &self.src[self.pos..]
    }

    pub(crate) fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    pub(crate) fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    pub(crate) fn eat(&mut self, c: char) -> bool {
        self.eat_str(c.encode_utf8(&mut [0; 4]))
    }

    pub(crate) fn eat_str(&mut self, s: &str) -> bool {
        let found = self.rest().starts_with(s);
        if found {
            self.pos += s.len();
        }
        found
    }

    /// Describes the next character for "expected X, found Y" messages.
    pub(crate) fn describe_next(&self) -> String {
        match self.peek() {
            None => "end of file".to_owned(),
            Some('\n') => "end of line".to_owned(),
            Some('\r') if self.rest().starts_with("\r\n") => "end of line".to_owned(),
            Some(c) => describe_char(c),
        }
    }

    pub(crate) fn error_here(&self, message: impl Into<String>) -> Diag {
        let width = self.peek().map_or(0, char::len_utf8);
        Diag::new(Span::new(self.pos, self.pos + width), message)
    }

    pub(crate) fn skip_ws(&mut self) {
        let rest = self.rest();
        self.pos += rest.len() - rest.trim_start_matches([' ', '\t']).len();
    }

    /// Consumes one line ending (LF or CRLF); returns whether there was one.
    pub(crate) fn eat_newline(&mut self) -> PResult<bool> {
        if self.eat('\n') || self.eat_str("\r\n") {
            return Ok(true);
        }
        if self.peek() == Some('\r') {
            return Err(self.error_here("carriage return must be followed by a newline"));
        }
        Ok(false)
    }

    /// Skips a `#` comment up to (not including) the line ending.
    pub(crate) fn skip_comment(&mut self) -> PResult<()> {
        if !self.eat('#') {
            return Ok(());
        }
        while let Some(c) = self.peek() {
            if c == '\n' || self.rest().starts_with("\r\n") {
                break;
            }
            if is_forbidden_control(c) {
                return Err(
                    self.error_here(format!("{} is not allowed in comments", describe_char(c)))
                );
            }
            self.bump();
        }
        Ok(())
    }

    /// Whitespace, comments and newlines: what may sit between array elements.
    pub(crate) fn skip_trivia(&mut self) -> PResult<()> {
        loop {
            self.skip_ws();
            self.skip_comment()?;
            if !self.eat_newline()? {
                return Ok(());
            }
        }
    }

    /// Requires the rest of the line to be blank or a comment, and consumes
    /// the line ending. `after` names what came before, for the message.
    pub(crate) fn end_of_line(&mut self, after: &str) -> PResult<()> {
        self.skip_ws();
        self.skip_comment()?;
        if self.peek().is_none() || self.eat_newline()? {
            return Ok(());
        }
        Err(self.error_here(format!(
            "expected end of line after {after}, found {}",
            self.describe_next()
        )))
    }

    /// A possibly dotted key: `a`, `a.b`, `"quoted key".c`, `a . 'b'`.
    /// Trailing whitespace is consumed.
    pub(crate) fn key(&mut self) -> PResult<Vec<KeySeg>> {
        let mut segs = vec![self.simple_key()?];
        loop {
            self.skip_ws();
            if !self.eat('.') {
                return Ok(segs);
            }
            self.skip_ws();
            segs.push(self.simple_key()?);
        }
    }

    pub(crate) fn simple_key(&mut self) -> PResult<KeySeg> {
        let start = self.pos;
        match self.peek() {
            Some('"') | Some('\'') => {
                if self.rest().starts_with("\"\"\"") || self.rest().starts_with("'''") {
                    return Err(self.error_here("multi-line strings cannot be used as keys"));
                }
                let name = self.string()?;
                Ok(KeySeg {
                    name,
                    span: Span::new(start, self.pos),
                })
            }
            Some(c) if is_bare_key_char(c) => {
                let len = self
                    .rest()
                    .bytes()
                    .take_while(|&b| is_bare_key_char(b as char))
                    .count();
                self.pos += len;
                Ok(KeySeg {
                    name: self.src[start..self.pos].to_owned(),
                    span: Span::new(start, self.pos),
                })
            }
            _ => Err(self.error_here(format!("expected a key, found {}", self.describe_next()))),
        }
    }

    /// Any of the four string forms; the cursor must be at the opening quote.
    pub(crate) fn string(&mut self) -> PResult<String> {
        let start = self.pos;
        if self.eat_str("\"\"\"") {
            self.multi_line(start, '"')
        } else if self.eat_str("'''") {
            self.multi_line(start, '\'')
        } else if self.eat('"') {
            self.single_line(start, '"')
        } else if self.eat('\'') {
            self.single_line(start, '\'')
        } else {
            Err(self.error_here(format!("expected a string, found {}", self.describe_next())))
        }
    }

    fn single_line(&mut self, start: usize, quote: char) -> PResult<String> {
        let mut out = String::new();
        loop {
            match self.peek() {
                None | Some('\n') => return Err(unterminated(start, 1)),
                Some('\r') if self.rest().starts_with("\r\n") => {
                    return Err(unterminated(start, 1))
                }
                Some(c) if c == quote => {
                    self.bump();
                    return Ok(out);
                }
                Some('\\') if quote == '"' => self.escape(&mut out)?,
                Some(c) if is_forbidden_control(c) => return Err(self.control_in_string(c)),
                Some(c) => {
                    out.push(c);
                    self.bump();
                }
            }
        }
    }

    fn multi_line(&mut self, start: usize, quote: char) -> PResult<String> {
        let mut out = String::new();
        // A newline right after the opening delimiter is not part of the value.
        self.eat_newline()?;
        loop {
            match self.peek() {
                None => return Err(unterminated(start, 3)),
                Some(c) if c == quote => {
                    let run = self.rest().chars().take_while(|&q| q == quote).count();
                    if run < 3 {
                        out.extend(std::iter::repeat(quote).take(run));
                        self.pos += run;
                        continue;
                    }
                    // Up to two quotes may directly precede the closing delimiter.
                    if run > 5 {
                        let at = self.pos + 5;
                        return Err(Diag::new(
                            Span::new(at, self.pos + run),
                            "too many quotes at the end of a multi-line string",
                        ));
                    }
                    out.extend(std::iter::repeat(quote).take(run - 3));
                    self.pos += run;
                    return Ok(out);
                }
                Some('\\') if quote == '"' => {
                    if !self.line_ending_backslash()? {
                        self.escape(&mut out)?;
                    }
                }
                Some('\n') | Some('\r') => {
                    // CRLF is normalised to LF so values do not depend on the
                    // line endings of the file they came from.
                    if !self.eat_newline()? {
                        return Err(self.control_in_string('\r'));
                    }
                    out.push('\n');
                }
                Some(c) if is_forbidden_control(c) => return Err(self.control_in_string(c)),
                Some(c) => {
                    out.push(c);
                    self.bump();
                }
            }
        }
    }

    /// In multi-line basic strings a backslash at the end of a line trims the
    /// line ending and the indentation of the next line.
    fn line_ending_backslash(&mut self) -> PResult<bool> {
        let after = &self.rest()[1..];
        let tail = after.trim_start_matches([' ', '\t']);
        if !(tail.starts_with('\n') || tail.starts_with("\r\n")) {
            return Ok(false);
        }
        self.pos += 1 + after.len() - tail.len();
        self.eat_newline()?;
        self.skip_ws();
        Ok(true)
    }

    fn escape(&mut self, out: &mut String) -> PResult<()> {
        let start = self.pos;
        self.bump();
        let c = match self.bump() {
            Some('b') => '\u{8}',
            Some('t') => '\t',
            Some('n') => '\n',
            Some('f') => '\u{c}',
            Some('r') => '\r',
            Some('"') => '"',
            Some('\\') => '\\',
            Some('u') => self.unicode_escape(start, 4)?,
            Some('U') => self.unicode_escape(start, 8)?,
            Some(other) if !other.is_control() => {
                return Err(Diag::new(
                    Span::new(start, self.pos),
                    format!("invalid escape sequence `\\{other}`"),
                ))
            }
            _ => {
                return Err(Diag::new(
                    Span::new(start, start + 1),
                    "incomplete escape sequence",
                ))
            }
        };
        out.push(c);
        Ok(())
    }

    fn unicode_escape(&mut self, start: usize, len: usize) -> PResult<char> {
        let digits = self
            .rest()
            .get(..len)
            .filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()));
        let Some(digits) = digits else {
            return Err(Diag::new(
                Span::new(start, self.pos),
                format!(
                    "expected {len} hex digits after `{}`",
                    &self.src[start..self.pos]
                ),
            ));
        };
        self.pos += len;
        let code = u32::from_str_radix(digits, 16).expect("digits were checked to be hex");
        char::from_u32(code).ok_or_else(|| {
            Diag::new(
                Span::new(start, self.pos),
                format!("U+{code:04X} is not a valid unicode scalar value"),
            )
        })
    }

    fn control_in_string(&self, c: char) -> Diag {
        self.error_here(format!("{} must be escaped in strings", describe_char(c)))
    }

    /// A boolean, number or date-time at the cursor.
    pub(crate) fn scalar(&mut self) -> PResult<Value> {
        let start = self.pos;
        let mut end = start + token_len(self.rest());
        // Date-times may separate date and time with a space: `1979-05-27 07:32:00`.
        let after = &self.src[end..];
        if is_date_prefix(&self.src[start..end]) && after.len() > 3 {
            let b = after.as_bytes();
            if b[0] == b' ' && b[1].is_ascii_digit() && b[2].is_ascii_digit() && b[3] == b':' {
                end += 1 + token_len(&after[1..]);
            }
        }
        if end == start {
            return Err(
                self.error_here(format!("expected a value, found {}", self.describe_next()))
            );
        }
        self.pos = end;
        classify_scalar(&self.src[start..end]).map_err(|m| Diag::new(Span::new(start, end), m))
    }
}

fn unterminated(start: usize, width: usize) -> Diag {
    Diag::new(Span::new(start, start + width), "unterminated string")
}

fn token_len(s: &str) -> usize {
    s.bytes()
        .take_while(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'-' | b'.' | b':'))
        .count()
}

fn is_date_prefix(tok: &str) -> bool {
    let b = tok.as_bytes();
    b.len() == 10 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-'
}

fn looks_like_datetime(tok: &str) -> bool {
    let b = tok.as_bytes();
    (b.len() >= 5 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-')
        || (b.len() >= 3 && b[..2].iter().all(u8::is_ascii_digit) && b[2] == b':')
}

/// Classifies a bare (unquoted) value token.
pub(crate) fn classify_scalar(tok: &str) -> Result<Value, String> {
    match tok {
        "true" => return Ok(Value::Boolean(true)),
        "false" => return Ok(Value::Boolean(false)),
        "inf" | "+inf" => return Ok(Value::Float(f64::INFINITY)),
        "-inf" => return Ok(Value::Float(f64::NEG_INFINITY)),
        "nan" | "+nan" | "-nan" => return Ok(Value::Float(f64::NAN)),
        _ => {}
    }
    if looks_like_datetime(tok) {
        return validate_datetime(tok).map(|()| Value::Datetime(tok.to_owned()));
    }
    if tok.starts_with(|c: char| c.is_ascii_digit() || c == '+' || c == '-') {
        return parse_number(tok);
    }
    Err(format!(
        "expected a value, found `{tok}` (strings must be quoted)"
    ))
}

/// Removes digit-separating underscores, checking that every character is a
/// digit of the radix and every underscore sits between two digits.
fn strip_underscores(digits: &str, radix: u32) -> Result<String, &'static str> {
    if digits.is_empty() {
        return Err("missing digits");
    }
    let chars: Vec<char> = digits.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            let between = i > 0
                && i + 1 < chars.len()
                && chars[i - 1].is_digit(radix)
                && chars[i + 1].is_digit(radix);
            if !between {
                return Err("underscores must sit between digits");
            }
        } else if !c.is_digit(radix) {
            return Err("unexpected character");
        }
    }
    Ok(digits.replace('_', ""))
}

/// Parses a TOML integer or float (not `inf`/`nan`).
pub(crate) fn parse_number(tok: &str) -> Result<Value, String> {
    let bad = |why: &str| format!("invalid number `{tok}`: {why}");
    let (sign, body) = match tok.as_bytes().first() {
        Some(b'+') => ("", &tok[1..]),
        Some(b'-') => ("-", &tok[1..]),
        _ => ("", tok),
    };
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(digits) = body.strip_prefix(prefix) {
            if body.len() != tok.len() {
                return Err(bad("only decimal integers may have a sign"));
            }
            let clean = strip_underscores(digits, radix).map_err(bad)?;
            return i64::from_str_radix(&clean, radix)
                .map(Value::Integer)
                .map_err(|_| format!("integer `{tok}` does not fit in 64 bits"));
        }
    }
    let no_leading_zero = |digits: &str| -> Result<(), String> {
        if digits.len() > 1 && digits.starts_with('0') {
            Err(bad("leading zeros are not allowed"))
        } else {
            Ok(())
        }
    };
    if !body.contains(['.', 'e', 'E']) {
        let clean = strip_underscores(body, 10).map_err(bad)?;
        no_leading_zero(&clean)?;
        return format!("{sign}{clean}")
            .parse()
            .map(Value::Integer)
            .map_err(|_| format!("integer `{tok}` does not fit in 64 bits"));
    }
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (int_part, fraction) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let int_digits = strip_underscores(int_part, 10).map_err(bad)?;
    no_leading_zero(&int_digits)?;
    let mut text = format!("{sign}{int_digits}");
    if let Some(fraction) = fraction {
        text.push('.');
        text.push_str(&strip_underscores(fraction, 10).map_err(bad)?);
    }
    if let Some(exponent) = exponent {
        let (esign, edigits) = match exponent.as_bytes().first() {
            Some(b'+') | Some(b'-') => exponent.split_at(1),
            _ => ("", exponent),
        };
        text.push('e');
        text.push_str(esign);
        text.push_str(&strip_underscores(edigits, 10).map_err(bad)?);
    }
    match text.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(Value::Float(f)),
        Ok(_) => Err(format!("float `{tok}` is out of range")),
        Err(_) => Err(bad("malformed float")),
    }
}

/// Validates an offset date-time, local date-time, local date or local time
/// (RFC 3339 as profiled by TOML). The text itself is kept as written.
pub(crate) fn validate_datetime(tok: &str) -> Result<(), String> {
    let bad = |why: String| format!("invalid date-time `{tok}`: {why}");
    let b = tok.as_bytes();
    if b.len() >= 3 && b[2] == b':' {
        return match time(tok).map_err(bad)? {
            "" => Ok(()),
            rest => Err(bad(format!("unexpected `{rest}` after local time"))),
        };
    }
    let date = tok
        .get(..10)
        .ok_or_else(|| bad("expected YYYY-MM-DD".into()))?;
    let (y, m, d) = match (num(date, 0, 4), num(date, 5, 2), num(date, 8, 2)) {
        (Some(y), Some(m), Some(d)) if &date[7..8] == "-" => (y, m, d),
        _ => return Err(bad("expected YYYY-MM-DD".into())),
    };
    if !(1..=12).contains(&m) {
        return Err(bad(format!("month {m} is out of range")));
    }
    if d < 1 || d > days_in_month(y, m) {
        return Err(bad(format!("day {d} is out of range for {y:04}-{m:02}")));
    }
    let rest = &tok[10..];
    let Some(sep) = rest.chars().next() else {
        return Ok(());
    };
    if !matches!(sep, 'T' | 't' | ' ') {
        return Err(bad(format!("unexpected `{rest}` after date")));
    }
    match time(&rest[1..]).map_err(bad)? {
        "" | "Z" | "z" => Ok(()),
        offset => {
            let b = offset.as_bytes();
            let valid = b.len() == 6
                && matches!(b[0], b'+' | b'-')
                && b[3] == b':'
                && num(offset, 1, 2).is_some_and(|h| h <= 23)
                && num(offset, 4, 2).is_some_and(|m| m <= 59);
            if valid {
                Ok(())
            } else {
                Err(bad(format!("invalid offset `{offset}`")))
            }
        }
    }
}

/// Checks `HH:MM:SS[.fraction]` at the start of `s`; returns what follows.
fn time(s: &str) -> Result<&str, String> {
    let (h, m, sec) = match (num(s, 0, 2), num(s, 3, 2), num(s, 6, 2)) {
        (Some(h), Some(m), Some(sec)) if &s[2..3] == ":" && &s[5..6] == ":" => (h, m, sec),
        _ => return Err("expected HH:MM:SS".into()),
    };
    // 60 is a leap second, which RFC 3339 allows.
    if h > 23 || m > 59 || sec > 60 {
        return Err(format!("time {h:02}:{m:02}:{sec:02} is out of range"));
    }
    let rest = &s[8..];
    match rest.strip_prefix('.') {
        Some(frac) => {
            let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return Err("expected digits after the decimal point".into());
            }
            Ok(&frac[digits..])
        }
        None => Ok(rest),
    }
}

fn num(s: &str, at: usize, len: usize) -> Option<u32> {
    let digits = s.get(at..at + len)?;
    if digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(src: &str) -> PResult<String> {
        Cursor::new(src).string()
    }

    fn scalar(src: &str) -> Result<Value, String> {
        Cursor::new(src).scalar().map_err(|d| d.message)
    }

    #[test]
    fn rejects_bad_escapes_and_control_characters() {
        let cases = [
            (r#""\x41""#, "invalid escape sequence `\\x`"),
            (r#""\u12G4""#, "expected 4 hex digits after `\\u`"),
            (r#""\uD800""#, "U+D800 is not a valid unicode scalar value"),
            (
                "\"bell\u{7}\"",
                "control character U+0007 must be escaped in strings",
            ),
            ("\"open\nx\"", "unterminated string"),
            (
                "'''x''''''",
                "too many quotes at the end of a multi-line string",
            ),
        ];
        for (src, message) in cases {
            assert_eq!(string(src).unwrap_err().message, message, "{src}");
        }
    }

    #[test]
    fn numbers_in_all_radixes_and_forms() {
        let cases = [
            ("1_000_000", Value::Integer(1_000_000)),
            ("+0", Value::Integer(0)),
            ("0xDEAD_beef", Value::Integer(0xdead_beef)),
            ("0o755", Value::Integer(0o755)),
            ("0b1101", Value::Integer(13)),
            ("-9223372036854775808", Value::Integer(i64::MIN)),
            ("-0.5e-3", Value::Float(-0.0005)),
            ("1e1_0", Value::Float(1e10)),
            ("6.626E+34", Value::Float(6.626e34)),
            ("-inf", Value::Float(f64::NEG_INFINITY)),
        ];
        for (src, value) in cases {
            assert_eq!(scalar(src), Ok(value), "{src}");
        }
        assert!(matches!(scalar("nan"), Ok(Value::Float(f)) if f.is_nan()));
    }

    #[test]
    fn rejects_malformed_numbers() {
        let cases = [
            ("0123", "leading zeros are not allowed"),
            ("03.1", "leading zeros are not allowed"),
            ("1__0", "underscores must sit between digits"),
            ("1_", "underscores must sit between digits"),
            ("-0x10", "only decimal integers may have a sign"),
            ("0b102", "unexpected character"),
            ("12ms", "unexpected character"),
            ("1.", "missing digits"),
            ("1e", "missing digits"),
        ];
        for (src, why) in cases {
            assert_eq!(
                scalar(src).unwrap_err(),
                format!("invalid number `{src}`: {why}")
            );
        }
        let big = "9223372036854775808";
        assert_eq!(
            scalar(big).unwrap_err(),
            format!("integer `{big}` does not fit in 64 bits")
        );
        assert_eq!(
            scalar("1e999").unwrap_err(),
            "float `1e999` is out of range"
        );
    }

    #[test]
    fn date_times_are_validated_and_kept_verbatim() {
        for ok in [
            "1979-05-27T07:32:00Z",
            "1979-05-27 07:32:00.999999-07:00",
            "1979-05-27t07:32:00z",
            "2024-02-29",
            "00:32:00.5",
            "2016-12-31T23:59:60Z",
        ] {
            assert_eq!(scalar(ok), Ok(Value::Datetime(ok.to_owned())), "{ok}");
        }
        let cases = [
            ("2023-02-29", "day 29 is out of range for 2023-02"),
            ("2023-13-01", "month 13 is out of range"),
            ("2023-01-01T25:00:00", "time 25:00:00 is out of range"),
            ("2023-01-01T10:00", "expected HH:MM:SS"),
            ("2023-01-01T10:00:00+5:00", "invalid offset `+5:00`"),
            ("10:00:00Z", "unexpected `Z` after local time"),
        ];
        for (src, why) in cases {
            let message = format!("invalid date-time `{src}`: {why}");
            assert_eq!(scalar(src).unwrap_err(), message);
        }
    }

    #[test]
    fn bare_words_are_not_values() {
        for word in ["yes", "True", "_1", ".5"] {
            let message = format!("expected a value, found `{word}` (strings must be quoted)");
            assert_eq!(scalar(word).unwrap_err(), message);
        }
        assert_eq!(scalar(",").unwrap_err(), "expected a value, found `,`");
    }

    #[test]
    fn dotted_keys_with_whitespace_and_quotes() {
        let mut c = Cursor::new("a . \"b.c\" .'d' = 1");
        let names: Vec<String> = c.key().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["a", "b.c", "d"]);
        assert_eq!(c.peek(), Some('='));
        assert!(Cursor::new("\"\"\"k\"\"\" = 1").key().is_err());
    }
}
