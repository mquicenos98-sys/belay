//! Strict RFC 8259 parsing, with opt-in leniency for hand-edited files.
//!
//! The parser is recursive descent over the UTF-8 bytes of a `&str`. Because
//! the input is already valid UTF-8 and every structural character is ASCII,
//! string contents between escapes are copied as whole slices without
//! re-validation. Recursion depth is bounded by
//! [`ParseOptions::max_depth`], so hostile input like a megabyte of `[` fails
//! cleanly instead of overflowing the stack.

mod error;

pub use error::{ParseError, ParseErrorKind};

use crate::number::{integer_from_digits, Number};
use crate::{Map, Value};

/// Parser settings. The default is strict RFC 8259 with a depth limit of 128.
///
/// ```
/// use belay_json::{parse_with, ParseOptions};
///
/// let src = "{\n  // retry quickly\n  \"max\": 3,\n}";
/// assert!(belay_json::parse(src).is_err());
/// let v = parse_with(src, &ParseOptions::relaxed()).unwrap();
/// assert_eq!(v["max"], 3);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ParseOptions {
    /// Maximum nesting of arrays and objects; `0` allows only scalars.
    pub max_depth: usize,
    /// Accept a single `,` before `]` or `}`.
    pub allow_trailing_commas: bool,
    /// Accept `// line` and `/* block */` comments wherever whitespace is
    /// allowed.
    pub allow_comments: bool,
    /// Reject objects that repeat a key. RFC 8259 leaves duplicates to the
    /// implementation; by default the last value wins and the key keeps its
    /// first position.
    pub reject_duplicate_keys: bool,
}

impl ParseOptions {
    /// Default nesting limit. Deep enough for any document belay produces,
    /// shallow enough that recursive drops and writes of the result are safe.
    pub const DEFAULT_MAX_DEPTH: usize = 128;

    /// Strict RFC 8259 (same as `default()`).
    #[must_use]
    pub fn strict() -> ParseOptions {
        ParseOptions {
            max_depth: Self::DEFAULT_MAX_DEPTH,
            allow_trailing_commas: false,
            allow_comments: false,
            reject_duplicate_keys: false,
        }
    }

    /// Comments and trailing commas allowed, for files people edit by hand
    /// (workflow definitions, fixtures).
    #[must_use]
    pub fn relaxed() -> ParseOptions {
        ParseOptions {
            allow_trailing_commas: true,
            allow_comments: true,
            ..ParseOptions::strict()
        }
    }

    /// Sets [`max_depth`](ParseOptions#structfield.max_depth).
    #[must_use]
    pub fn max_depth(mut self, depth: usize) -> ParseOptions {
        self.max_depth = depth;
        self
    }

    /// Sets [`allow_trailing_commas`](ParseOptions#structfield.allow_trailing_commas).
    #[must_use]
    pub fn allow_trailing_commas(mut self, allow: bool) -> ParseOptions {
        self.allow_trailing_commas = allow;
        self
    }

    /// Sets [`allow_comments`](ParseOptions#structfield.allow_comments).
    #[must_use]
    pub fn allow_comments(mut self, allow: bool) -> ParseOptions {
        self.allow_comments = allow;
        self
    }

    /// Sets [`reject_duplicate_keys`](ParseOptions#structfield.reject_duplicate_keys).
    #[must_use]
    pub fn reject_duplicate_keys(mut self, reject: bool) -> ParseOptions {
        self.reject_duplicate_keys = reject;
        self
    }
}

impl Default for ParseOptions {
    fn default() -> ParseOptions {
        ParseOptions::strict()
    }
}

/// Parses a complete JSON document with strict options.
///
/// ```
/// let v = belay_json::parse(r#"{"id": 18446744073709551615, "ratio": 0.5}"#).unwrap();
/// assert_eq!(v["id"].as_u64(), Some(u64::MAX));
/// assert_eq!(v["ratio"].as_f64(), Some(0.5));
///
/// let err = belay_json::parse("[1, 2,]").unwrap_err();
/// assert_eq!(err.to_string(), "trailing comma is not allowed at line 1, column 6");
/// ```
pub fn parse(src: &str) -> Result<Value, ParseError> {
    parse_with(src, &ParseOptions::strict())
}

/// Parses a complete JSON document with the given options.
pub fn parse_with(src: &str, options: &ParseOptions) -> Result<Value, ParseError> {
    Parser::new(src, options).document()
}

/// Parses a document from raw bytes, which must be UTF-8 (RFC 8259 §8.1).
pub fn parse_bytes(bytes: &[u8]) -> Result<Value, ParseError> {
    parse_bytes_with(bytes, &ParseOptions::strict())
}

/// Parses a document from raw bytes with the given options.
pub fn parse_bytes_with(bytes: &[u8], options: &ParseOptions) -> Result<Value, ParseError> {
    match std::str::from_utf8(bytes) {
        Ok(src) => parse_with(src, options),
        Err(e) => {
            let valid = &bytes[..e.valid_up_to()];
            // The prefix is valid by definition; it only serves to compute
            // line and column of the first bad byte.
            let prefix = std::str::from_utf8(valid).unwrap_or("");
            Err(ParseError::at(
                ParseErrorKind::InvalidUtf8,
                prefix,
                valid.len(),
            ))
        }
    }
}

/// Parses exactly one number literal (for `Number: FromStr`).
pub(crate) fn parse_number_literal(src: &str) -> Result<Number, ParseError> {
    let options = ParseOptions::strict();
    let mut parser = Parser::new(src, &options);
    match parser.peek() {
        Some(b'-' | b'0'..=b'9') => {}
        Some(_) => return Err(parser.unexpected("a number")),
        None => {
            return Err(parser.error(ParseErrorKind::UnexpectedEof {
                expected: "a number",
            }))
        }
    }
    let number = parser.number()?;
    if parser.pos < src.len() {
        return Err(parser.error(ParseErrorKind::TrailingCharacters));
    }
    Ok(number)
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
    options: &'a ParseOptions,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, options: &'a ParseOptions) -> Parser<'a> {
        Parser {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            depth: 0,
            options,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn error(&self, kind: ParseErrorKind) -> ParseError {
        self.error_at(kind, self.pos)
    }

    fn error_at(&self, kind: ParseErrorKind, offset: usize) -> ParseError {
        ParseError::at(kind, self.src, offset)
    }

    /// Error for whatever is at the current position, which is either the
    /// end of input or a character that does not fit.
    fn unexpected(&self, expected: &'static str) -> ParseError {
        match self.src[self.pos..].chars().next() {
            Some(found) => self.error(ParseErrorKind::UnexpectedChar { found, expected }),
            None => self.error(ParseErrorKind::UnexpectedEof { expected }),
        }
    }

    fn document(mut self) -> Result<Value, ParseError> {
        if self.src.starts_with('\u{feff}') {
            return Err(self.error(ParseErrorKind::ByteOrderMark));
        }
        self.skip_whitespace()?;
        let value = self.value()?;
        self.skip_whitespace()?;
        if self.pos < self.bytes.len() {
            return Err(self.error(ParseErrorKind::TrailingCharacters));
        }
        Ok(value)
    }

    fn skip_whitespace(&mut self) -> Result<(), ParseError> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                Some(b'/') if self.options.allow_comments => self.skip_comment()?,
                _ => return Ok(()),
            }
        }
    }

    fn skip_comment(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        match self.bytes.get(self.pos + 1) {
            Some(b'/') => {
                self.pos = match self.src[start..].find('\n') {
                    Some(i) => start + i + 1,
                    None => self.bytes.len(),
                };
                Ok(())
            }
            Some(b'*') => match self.src[start + 2..].find("*/") {
                Some(i) => {
                    self.pos = start + 2 + i + 2;
                    Ok(())
                }
                None => Err(self.error_at(ParseErrorKind::UnterminatedComment, start)),
            },
            _ => {
                self.pos += 1;
                Err(self.unexpected("'/' or '*' to start a comment"))
            }
        }
    }

    /// Parses a value starting at the current (non-whitespace) position.
    fn value(&mut self) -> Result<Value, ParseError> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number().map(Value::Number),
            _ => Err(self.unexpected("a value")),
        }
    }

    fn literal(&mut self, text: &'static str, value: Value) -> Result<Value, ParseError> {
        if self.src[self.pos..].starts_with(text) {
            self.pos += text.len();
            Ok(value)
        } else {
            Err(self.error(ParseErrorKind::InvalidLiteral { expected: text }))
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        if self.depth >= self.options.max_depth {
            return Err(self.error(ParseErrorKind::DepthLimitExceeded(self.options.max_depth)));
        }
        self.depth += 1;
        Ok(())
    }

    /// After a `,` inside a container: skips whitespace and reports whether
    /// the container closes right away, i.e. the comma was a trailing one.
    fn trailing_comma(&mut self, comma: usize, close: u8) -> Result<bool, ParseError> {
        self.skip_whitespace()?;
        if self.peek() != Some(close) {
            return Ok(false);
        }
        if self.options.allow_trailing_commas {
            self.pos += 1;
            Ok(true)
        } else {
            Err(self.error_at(ParseErrorKind::TrailingComma, comma))
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_whitespace()?;
        if self.peek() == Some(b']') {
            self.pos += 1;
        } else {
            loop {
                items.push(self.value()?);
                self.skip_whitespace()?;
                match self.peek() {
                    Some(b',') => {
                        let comma = self.pos;
                        self.pos += 1;
                        if self.trailing_comma(comma, b']')? {
                            break;
                        }
                    }
                    Some(b']') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(self.unexpected("',' or ']'")),
                }
            }
        }
        self.depth -= 1;
        Ok(Value::Array(items))
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.enter()?;
        self.pos += 1;
        let mut map = Map::new();
        self.skip_whitespace()?;
        if self.peek() == Some(b'}') {
            self.pos += 1;
        } else {
            loop {
                if self.peek() != Some(b'"') {
                    return Err(self.unexpected("a string key"));
                }
                let key_pos = self.pos;
                let key = self.string()?;
                if self.options.reject_duplicate_keys && map.contains_key(&key) {
                    return Err(self.error_at(ParseErrorKind::DuplicateKey(key), key_pos));
                }
                self.skip_whitespace()?;
                if self.peek() != Some(b':') {
                    return Err(self.unexpected("':' after object key"));
                }
                self.pos += 1;
                self.skip_whitespace()?;
                let value = self.value()?;
                map.insert(key, value);
                self.skip_whitespace()?;
                match self.peek() {
                    Some(b',') => {
                        let comma = self.pos;
                        self.pos += 1;
                        if self.trailing_comma(comma, b'}')? {
                            break;
                        }
                    }
                    Some(b'}') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(self.unexpected("',' or '}'")),
                }
            }
        }
        self.depth -= 1;
        Ok(Value::Object(map))
    }

    fn string(&mut self) -> Result<String, ParseError> {
        let open = self.pos;
        self.pos += 1;
        let mut out = String::new();
        let mut run = self.pos;
        loop {
            match self.peek() {
                None => return Err(self.error_at(ParseErrorKind::UnterminatedString, open)),
                Some(b'"') => {
                    out.push_str(&self.src[run..self.pos]);
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    out.push_str(&self.src[run..self.pos]);
                    self.escape(&mut out)?;
                    run = self.pos;
                }
                Some(c @ 0x00..=0x1f) => {
                    return Err(self.error(ParseErrorKind::ControlCharacter(char::from(c))));
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// Decodes one escape sequence starting at the backslash.
    fn escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        let backslash = self.pos;
        self.pos += 1;
        let decoded = match self.peek() {
            Some(b'"') => '"',
            Some(b'\\') => '\\',
            Some(b'/') => '/',
            Some(b'b') => '\u{8}',
            Some(b'f') => '\u{c}',
            Some(b'n') => '\n',
            Some(b'r') => '\r',
            Some(b't') => '\t',
            Some(b'u') => {
                self.pos += 1;
                let c = self.unicode_escape(backslash)?;
                out.push(c);
                return Ok(());
            }
            Some(_) => {
                let c = self.src[self.pos..].chars().next().unwrap_or('\u{fffd}');
                return Err(self.error_at(ParseErrorKind::InvalidEscape(c), backslash));
            }
            None => return Err(self.error(ParseErrorKind::UnterminatedString)),
        };
        self.pos += 1;
        out.push(decoded);
        Ok(())
    }

    /// Decodes the code point of a `\uXXXX` escape whose hex digits start at
    /// the current position, combining UTF-16 surrogate pairs. Unpaired
    /// surrogates are rejected: they cannot be represented in a Rust `String`
    /// and silently replacing them would corrupt data (RFC 8259 §8.2).
    fn unicode_escape(&mut self, backslash: usize) -> Result<char, ParseError> {
        let unit = self.hex4(backslash)?;
        let scalar = match unit {
            0xd800..=0xdbff => {
                if !self.src[self.pos..].starts_with("\\u") {
                    return Err(self.error_at(ParseErrorKind::LoneSurrogate(unit), backslash));
                }
                let second_backslash = self.pos;
                self.pos += 2;
                let low = self.hex4(second_backslash)?;
                if !(0xdc00..=0xdfff).contains(&low) {
                    return Err(self.error_at(ParseErrorKind::LoneSurrogate(unit), backslash));
                }
                0x10000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(low) - 0xdc00)
            }
            0xdc00..=0xdfff => {
                return Err(self.error_at(ParseErrorKind::LoneSurrogate(unit), backslash));
            }
            _ => u32::from(unit),
        };
        // Surrogates were handled above, so every remaining value is a
        // Unicode scalar value.
        char::from_u32(scalar)
            .ok_or_else(|| self.error_at(ParseErrorKind::InvalidUnicodeEscape, backslash))
    }

    fn hex4(&mut self, backslash: usize) -> Result<u16, ParseError> {
        let digits = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.error_at(ParseErrorKind::InvalidUnicodeEscape, backslash))?;
        let mut unit: u16 = 0;
        for &d in digits {
            let nibble = char::from(d)
                .to_digit(16)
                .ok_or_else(|| self.error_at(ParseErrorKind::InvalidUnicodeEscape, backslash))?;
            unit = (unit << 4) | nibble as u16;
        }
        self.pos += 4;
        Ok(unit)
    }

    /// Parses `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`.
    fn number(&mut self) -> Result<Number, ParseError> {
        let start = self.pos;
        let negative = self.peek() == Some(b'-');
        if negative {
            self.pos += 1;
        }
        let int_start = self.pos;
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error_at(
                        ParseErrorKind::InvalidNumber {
                            reason: "leading zeros are not allowed",
                        },
                        start,
                    ));
                }
            }
            Some(b'1'..=b'9') => self.digits(),
            _ => {
                return Err(self.error(ParseErrorKind::InvalidNumber {
                    reason: "expected a digit after '-'",
                }))
            }
        }
        let int_end = self.pos;
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            self.require_digit("expected a digit after the decimal point")?;
            is_float = true;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            self.require_digit("expected a digit in the exponent")?;
            is_float = true;
        }
        if !is_float {
            if let Some(n) = integer_from_digits(&self.bytes[int_start..int_end], negative) {
                return Ok(n);
            }
        }
        // The grammar was checked above, and it is a subset of what
        // `f64::from_str` accepts; the result is correctly rounded.
        match self.src[start..self.pos].parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Number::float_unchecked(f)),
            _ => Err(self.error_at(ParseErrorKind::NumberOutOfRange, start)),
        }
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }

    fn require_digit(&mut self, reason: &'static str) -> Result<(), ParseError> {
        if !matches!(self.peek(), Some(b'0'..=b'9')) {
            return Err(self.error(ParseErrorKind::InvalidNumber { reason }));
        }
        self.digits();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn kind_of(src: &str) -> ParseErrorKind {
        parse(src).unwrap_err().kind().clone()
    }

    #[test]
    fn parses_scalars_at_top_level() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse(" true ").unwrap(), Value::Bool(true));
        assert_eq!(parse("\"x\"").unwrap(), json!("x"));
        assert_eq!(parse("-12").unwrap(), json!(-12));
    }

    #[test]
    fn keeps_integers_exact_and_falls_back_to_float() {
        assert_eq!(
            parse("18446744073709551615").unwrap().as_u64(),
            Some(u64::MAX)
        );
        assert_eq!(
            parse("-9223372036854775808").unwrap().as_i64(),
            Some(i64::MIN)
        );
        let wide = parse("18446744073709551616").unwrap();
        assert!(wide.is_f64());
        assert_eq!(wide.as_f64(), Some(18446744073709551616.0));
        let wide_neg = parse("-9223372036854775809").unwrap();
        assert!(wide_neg.is_f64());
        // 2^53 + 1 is not representable as f64 but must survive as integer.
        assert_eq!(
            parse("9007199254740993").unwrap().as_u64(),
            Some(9_007_199_254_740_993)
        );
    }

    #[test]
    fn negative_zero_is_a_float_with_sign() {
        let v = parse("-0").unwrap();
        assert!(v.is_f64());
        assert!(v.as_f64().unwrap().is_sign_negative());
        assert_eq!(parse("0").unwrap().as_u64(), Some(0));
    }

    #[test]
    fn number_grammar_errors_have_reasons() {
        let reason = |src: &str| match kind_of(src) {
            ParseErrorKind::InvalidNumber { reason } => reason,
            other => panic!("{src}: expected InvalidNumber, got {other:?}"),
        };
        assert_eq!(reason("012"), "leading zeros are not allowed");
        assert_eq!(reason("-01"), "leading zeros are not allowed");
        assert_eq!(reason("-"), "expected a digit after '-'");
        assert_eq!(reason("-x"), "expected a digit after '-'");
        assert_eq!(reason("1."), "expected a digit after the decimal point");
        assert_eq!(reason("1.e5"), "expected a digit after the decimal point");
        assert_eq!(reason("1e"), "expected a digit in the exponent");
        assert_eq!(reason("1e+"), "expected a digit in the exponent");
        assert_eq!(kind_of("1e400"), ParseErrorKind::NumberOutOfRange);
        assert_eq!(kind_of("-1e400"), ParseErrorKind::NumberOutOfRange);
        assert_eq!(parse("1e-400").unwrap().as_f64(), Some(0.0));
    }

    #[test]
    fn decodes_escapes_and_surrogate_pairs() {
        let v = parse(r#""\"\\\/\b\f\n\r\t\u00e9\uD83D\uDE00""#).unwrap();
        assert_eq!(v, json!("\"\\/\u{8}\u{c}\n\r\té\u{1f600}"));
        assert_eq!(parse(r#""\u0000""#).unwrap(), json!("\u{0}"));
    }

    #[test]
    fn rejects_unpaired_surrogates() {
        assert_eq!(
            kind_of(r#""\uD800""#),
            ParseErrorKind::LoneSurrogate(0xd800)
        );
        assert_eq!(
            kind_of(r#""\uD800x""#),
            ParseErrorKind::LoneSurrogate(0xd800)
        );
        assert_eq!(
            kind_of(r#""\uD800\u0041""#),
            ParseErrorKind::LoneSurrogate(0xd800)
        );
        assert_eq!(
            kind_of(r#""\uD800\uD800""#),
            ParseErrorKind::LoneSurrogate(0xd800)
        );
        assert_eq!(
            kind_of(r#""\uDC00""#),
            ParseErrorKind::LoneSurrogate(0xdc00)
        );
        assert_eq!(
            kind_of(r#""\uD800\u12""#),
            ParseErrorKind::InvalidUnicodeEscape
        );
    }

    #[test]
    fn rejects_raw_control_characters_in_strings() {
        assert_eq!(kind_of("\"a\nb\""), ParseErrorKind::ControlCharacter('\n'));
        assert_eq!(
            kind_of("\"\u{1f}\""),
            ParseErrorKind::ControlCharacter('\u{1f}')
        );
        assert_eq!(parse("\"\u{7f}\"").unwrap(), json!("\u{7f}"));
    }

    #[test]
    fn error_positions_point_at_the_problem() {
        let err = parse("{\n  \"a\": 1,\n  \"b\" 2\n}").unwrap_err();
        assert_eq!((err.line(), err.column()), (3, 7));
        assert_eq!(
            err.to_string(),
            "unexpected character '2', expected ':' after object key at line 3, column 7"
        );
        let err = parse("[\"abc").unwrap_err();
        assert_eq!(err.kind(), &ParseErrorKind::UnterminatedString);
        assert_eq!(err.column(), 2, "points at the opening quote");
        let err = parse(r#"["\x"]"#).unwrap_err();
        assert_eq!(err.kind(), &ParseErrorKind::InvalidEscape('x'));
        assert_eq!(err.column(), 3, "points at the backslash");
    }

    #[test]
    fn depth_limit_is_enforced_exactly() {
        let nested = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        assert!(parse(&nested(128)).is_ok());
        assert_eq!(
            kind_of(&nested(129)),
            ParseErrorKind::DepthLimitExceeded(128)
        );
        let opts = ParseOptions::strict().max_depth(2);
        assert!(parse_with(r#"{"a": [1]}"#, &opts).is_ok());
        assert!(parse_with(r#"{"a": [[1]]}"#, &opts).is_err());
        let scalars_only = ParseOptions::strict().max_depth(0);
        assert!(parse_with("1", &scalars_only).is_ok());
        assert!(parse_with("[]", &scalars_only).is_err());
    }

    #[test]
    fn trailing_commas_only_with_option() {
        assert_eq!(kind_of("[1,]"), ParseErrorKind::TrailingComma);
        assert_eq!(kind_of(r#"{"a":1,}"#), ParseErrorKind::TrailingComma);
        let lenient = ParseOptions::strict().allow_trailing_commas(true);
        assert_eq!(parse_with("[1, ]", &lenient).unwrap(), json!([1]));
        assert_eq!(
            parse_with(r#"{"a":1 , }"#, &lenient).unwrap(),
            json!({"a": 1})
        );
        // Only one trailing comma, and never an empty element.
        assert!(parse_with("[1,,]", &lenient).is_err());
        assert!(parse_with("[,]", &lenient).is_err());
        assert!(parse_with("{,}", &lenient).is_err());
    }

    #[test]
    fn comments_only_with_option() {
        let src = "/* head */ [1, // one\n 2 /* two */] // tail";
        assert!(parse(src).is_err());
        let with_comments = ParseOptions::strict().allow_comments(true);
        assert_eq!(parse_with(src, &with_comments).unwrap(), json!([1, 2]));
        assert_eq!(
            parse_with("[1 /* open", &with_comments).unwrap_err().kind(),
            &ParseErrorKind::UnterminatedComment
        );
        assert!(parse_with("[1 / 2]", &with_comments).is_err());
        assert_eq!(parse_with("// only\n7", &with_comments).unwrap(), json!(7));
    }

    #[test]
    fn duplicate_keys_last_wins_unless_rejected() {
        let v = parse(r#"{"a": 1, "b": 2, "a": 3}"#).unwrap();
        assert_eq!(v, json!({"a": 3, "b": 2}));
        assert_eq!(v.as_object().unwrap().get_index(0), Some(("a", &json!(3))));
        let strict = ParseOptions::strict().reject_duplicate_keys(true);
        let err = parse_with(r#"{"a": 1, "a": 3}"#, &strict).unwrap_err();
        assert_eq!(err.kind(), &ParseErrorKind::DuplicateKey("a".into()));
        assert_eq!(err.column(), 10);
        // Nested objects have their own key space.
        assert!(parse_with(r#"{"a": {"a": 1}, "b": {"a": 2}}"#, &strict).is_ok());
    }

    #[test]
    fn rejects_byte_order_mark_and_invalid_utf8() {
        assert_eq!(kind_of("\u{feff}{}"), ParseErrorKind::ByteOrderMark);
        let err = parse_bytes(b"[\"ok\",\n \"\xff\"]").unwrap_err();
        assert_eq!(err.kind(), &ParseErrorKind::InvalidUtf8);
        assert_eq!((err.line(), err.column()), (2, 3));
        assert_eq!(parse_bytes(b"[1]").unwrap(), json!([1]));
    }

    #[test]
    fn large_objects_parse_in_linear_time() {
        // Regression guard for quadratic duplicate checks: 50k keys must be
        // fast even in debug builds.
        let src = format!(
            "{{{}}}",
            (0..50_000)
                .map(|i| format!("\"k{i}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let v = parse(&src).unwrap();
        assert_eq!(v.as_object().map(Map::len), Some(50_000));
        assert_eq!(v["k49999"], 49_999);
    }
}
