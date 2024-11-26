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
