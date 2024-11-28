//! Serialization to compact or pretty JSON text.
//!
//! One emitter, generic over [`fmt::Write`], backs every output path: `String`
//! buffers, `Display` for [`Value`], and `io::Write` sinks through a small
//! buffering adapter. Output is always valid RFC 8259 JSON: strings escape `"`,
//! `\` and all control characters; floats use the shortest representation
//! that parses back to the same `f64` (see [`Number`](crate::Number)).

use std::fmt;
use std::io;

use crate::number::write_f64;
use crate::{Map, Value};

/// Output settings.
///
/// ```
/// use belay_json::{json, WriteOptions};
///
/// let v = json!({"b": 1, "a": "é"});
/// let opts = WriteOptions::compact().sort_keys(true).ascii_only(true);
/// assert_eq!(v.to_string_with(&opts), r#"{"a":"\u00e9","b":1}"#);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct WriteOptions {
    /// Spaces per nesting level; `None` writes everything on one line.
    pub indent: Option<usize>,
    /// Write object keys in bytewise order instead of insertion order, for
    /// canonical output (diffs, hashing, golden files).
    pub sort_keys: bool,
    /// Escape every non-ASCII character as `\uXXXX` (surrogate pairs above
    /// U+FFFF), for transports that are not 8-bit clean.
    pub ascii_only: bool,
}

impl WriteOptions {
    /// Single-line output, insertion order, UTF-8 (same as `default()`).
    #[must_use]
    pub fn compact() -> WriteOptions {
        WriteOptions::default()
    }

    /// Two-space indentation.
    #[must_use]
    pub fn pretty() -> WriteOptions {
        WriteOptions {
            indent: Some(2),
            ..WriteOptions::default()
        }
    }

    /// Sets [`indent`](WriteOptions#structfield.indent).
    #[must_use]
    pub fn indent(mut self, spaces: Option<usize>) -> WriteOptions {
        self.indent = spaces;
        self
    }

    /// Sets [`sort_keys`](WriteOptions#structfield.sort_keys).
    #[must_use]
    pub fn sort_keys(mut self, sort: bool) -> WriteOptions {
        self.sort_keys = sort;
        self
    }

    /// Sets [`ascii_only`](WriteOptions#structfield.ascii_only).
    #[must_use]
    pub fn ascii_only(mut self, ascii: bool) -> WriteOptions {
        self.ascii_only = ascii;
        self
    }
}

/// Serializes to compact JSON.
///
/// ```
/// use belay_json::json;
///
/// let v = json!({"ok": true, "ratio": 1.0, "tags": []});
/// assert_eq!(belay_json::to_string(&v), r#"{"ok":true,"ratio":1.0,"tags":[]}"#);
/// ```
#[must_use]
pub fn to_string(value: &Value) -> String {
    to_string_with(value, &WriteOptions::compact())
}
