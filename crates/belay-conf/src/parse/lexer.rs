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
