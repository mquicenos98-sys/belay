//! Percent-encoding (RFC 3986 §2.1) for URL components, plus the
//! `application/x-www-form-urlencoded` flavour used by query strings.
//!
//! Encoding always works on UTF-8 bytes and emits uppercase hex digits, as
//! RFC 3986 §2.1 recommends. Decoding comes in a strict variant for places
//! where a bad escape should be an error (route parameters) and lenient ones
//! for places where it should not (query strings from arbitrary clients).

use std::fmt;

/// Which characters survive encoding unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeSet {
    /// Only unreserved characters (`A-Z a-z 0-9 - . _ ~`). Safe anywhere,
    /// including a single path segment.
    Component,
    /// Characters valid in a path, including `/`: unreserved, sub-delims,
    /// `:` and `@`.
    Path,
    /// Characters that may appear literally inside a query key or value;
    /// `&`, `=`, `+` and `#` are encoded because they carry structure.
    QueryValue,
}

impl EncodeSet {
    fn keeps(self, b: u8) -> bool {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        unreserved
            || match self {
                EncodeSet::Component => false,
                EncodeSet::Path => matches!(
                    b,
                    b'/' | b'!'
                        | b'$'
                        | b'&'
                        | b'\''
                        | b'('
                        | b')'
                        | b'*'
                        | b'+'
                        | b','
                        | b';'
                        | b'='
                        | b':'
                        | b'@'
                ),
                EncodeSet::QueryValue => matches!(
                    b,
                    b'!' | b'$'
                        | b'\''
                        | b'('
                        | b')'
                        | b'*'
                        | b','
                        | b';'
                        | b':'
                        | b'@'
                        | b'/'
                        | b'?'
                ),
            }
    }
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Percent-encodes every byte of `input` that `set` does not keep.
///
/// ```
/// use belay_http::percent::{encode, EncodeSet};
///
/// assert_eq!(encode("a b/c", EncodeSet::Component), "a%20b%2Fc");
/// assert_eq!(encode("a b/c", EncodeSet::Path), "a%20b/c");
/// assert_eq!(encode("x=1&y", EncodeSet::QueryValue), "x%3D1%26y");
/// ```
pub fn encode(input: &str, set: EncodeSet) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if set.keeps(b) {
            out.push(b as char);
        } else {
            push_escape(&mut out, b);
        }
    }
    out
}
