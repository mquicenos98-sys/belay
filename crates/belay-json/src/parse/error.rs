//! Parse errors with source positions.

use std::fmt;

/// Why a document was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseErrorKind {
    /// The input ended early; `expected` names what should have come next.
    UnexpectedEof {
        /// What the parser was looking for, e.g. `"',' or ']'"`.
        expected: &'static str,
    },
    /// A character that cannot start or continue the current construct.
    UnexpectedChar {
        /// The offending character.
        found: char,
        /// What the parser was looking for.
        expected: &'static str,
    },
    /// Something other than whitespace follows the top-level value.
    TrailingCharacters,
    /// A `,` directly before `]` or `}` without
    /// [`allow_trailing_commas`](crate::ParseOptions::allow_trailing_commas).
    TrailingComma,
    /// A misspelled or truncated `true`, `false` or `null`.
    InvalidLiteral {
        /// The literal that was started.
        expected: &'static str,
    },
    /// A number that violates the JSON grammar.
    InvalidNumber {
        /// What exactly is wrong, e.g. "leading zeros are not allowed".
        reason: &'static str,
    },
    /// A number whose magnitude overflows `f64`.
    NumberOutOfRange,
    /// A raw control character (U+0000 to U+001F) inside a string.
    ControlCharacter(char),
    /// A backslash followed by a character that is not a valid escape.
    InvalidEscape(char),
    /// `\u` not followed by four hex digits.
    InvalidUnicodeEscape,
    /// A UTF-16 surrogate escape without its partner, e.g. `"\uD800"`.
    LoneSurrogate(u16),
    /// The input ended inside a string.
    UnterminatedString,
    /// The input ended inside a `/* */` comment.
    UnterminatedComment,
    /// Arrays and objects are nested deeper than the configured limit.
    DepthLimitExceeded(usize),
    /// A key appeared twice with
    /// [`reject_duplicate_keys`](crate::ParseOptions::reject_duplicate_keys).
    DuplicateKey(String),
    /// The input bytes are not valid UTF-8.
    InvalidUtf8,
    /// The input starts with U+FEFF, which RFC 8259 §8.1 forbids senders
    /// to add.
    ByteOrderMark,
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseErrorKind::UnexpectedEof { expected } => {
                write!(f, "unexpected end of input, expected {expected}")
            }
            ParseErrorKind::UnexpectedChar { found, expected } => {
                write!(f, "unexpected character {found:?}, expected {expected}")
            }
            ParseErrorKind::TrailingCharacters => {
                f.write_str("unexpected characters after the top-level value")
            }
            ParseErrorKind::TrailingComma => f.write_str("trailing comma is not allowed"),
            ParseErrorKind::InvalidLiteral { expected } => {
                write!(f, "invalid literal, expected `{expected}`")
            }
            ParseErrorKind::InvalidNumber { reason } => write!(f, "invalid number: {reason}"),
            ParseErrorKind::NumberOutOfRange => {
                f.write_str("number is too large to represent as a 64-bit float")
            }
            ParseErrorKind::ControlCharacter(c) => write!(
                f,
                "unescaped control character U+{:04X} in string",
                u32::from(*c)
            ),
            ParseErrorKind::InvalidEscape(c) => write!(f, "invalid escape sequence \\{c}"),
            ParseErrorKind::InvalidUnicodeEscape => {
                f.write_str("invalid \\u escape, expected four hex digits")
            }
            ParseErrorKind::LoneSurrogate(unit) => {
                write!(f, "unpaired surrogate \\u{unit:04X} in string")
            }
            ParseErrorKind::UnterminatedString => f.write_str("unterminated string"),
            ParseErrorKind::UnterminatedComment => f.write_str("unterminated block comment"),
            ParseErrorKind::DepthLimitExceeded(limit) => {
                write!(f, "nesting exceeds the maximum depth of {limit}")
            }
            ParseErrorKind::DuplicateKey(key) => write!(f, "duplicate object key {key:?}"),
            ParseErrorKind::InvalidUtf8 => f.write_str("invalid UTF-8"),
            ParseErrorKind::ByteOrderMark => f.write_str("byte order mark is not allowed"),
        }
    }
}

/// A rejected document: what went wrong and where.
///
/// `Display` reads like `unexpected character 'x', expected ',' or ']' at line
/// 3, column 14`. Lines and columns are 1-based; columns count characters, not
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    kind: ParseErrorKind,
    offset: usize,
    line: usize,
    column: usize,
}

impl ParseError {
    /// Builds an error for byte `offset` of `src`, resolving line and column.
    /// Only called on the error path, so the rescan of the prefix is fine.
    pub(crate) fn at(kind: ParseErrorKind, src: &str, offset: usize) -> ParseError {
        let mut offset = offset.min(src.len());
        while !src.is_char_boundary(offset) {
            offset -= 1;
        }
        let before = &src[..offset];
        let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let column = before[line_start..].len() + 1;
        ParseError {
            kind,
            offset,
            line,
            column,
        }
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> &ParseErrorKind {
        &self.kind
    }

    /// Byte offset of the problem in the input.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// 1-based line of the problem.
    #[must_use]
    pub fn line(&self) -> usize {
        self.line
    }

    /// 1-based column (in characters) of the problem.
    #[must_use]
    pub fn column(&self) -> usize {
        self.column
    }
}
