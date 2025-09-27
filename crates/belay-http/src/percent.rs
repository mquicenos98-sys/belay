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

/// Shorthand for [`encode`] with [`EncodeSet::Component`].
pub fn encode_component(input: &str) -> String {
    encode(input, EncodeSet::Component)
}

pub(crate) fn push_escape(out: &mut String, b: u8) {
    out.push('%');
    out.push(HEX[usize::from(b >> 4)] as char);
    out.push(HEX[usize::from(b & 0x0f)] as char);
}

pub(crate) fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Why strict decoding failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A `%` not followed by two hex digits, at this byte offset.
    InvalidEscape {
        /// Offset of the `%`.
        offset: usize,
    },
    /// The decoded bytes are not UTF-8.
    InvalidUtf8,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::InvalidEscape { offset } => {
                write!(f, "invalid percent-escape at byte {offset}")
            }
            DecodeError::InvalidUtf8 => f.write_str("percent-decoded bytes are not valid utf-8"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decodes `%XX` escapes, failing on malformed escapes.
pub fn decode_bytes(input: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == b'%' {
            let hi = input.get(i + 1).copied().and_then(hex_value);
            let lo = input.get(i + 2).copied().and_then(hex_value);
            match (hi, lo) {
                (Some(hi), Some(lo)) => {
                    out.push(hi << 4 | lo);
                    i += 3;
                    continue;
                }
                _ => return Err(DecodeError::InvalidEscape { offset: i }),
            }
        }
        out.push(b);
        i += 1;
    }
    Ok(out)
}

/// Decodes `%XX` escapes into a UTF-8 string, failing on malformed escapes
/// or invalid UTF-8.
///
/// ```
/// use belay_http::percent::decode;
///
/// assert_eq!(decode("caf%C3%A9%20au%20lait").as_deref(), Ok("café au lait"));
/// assert!(decode("100%").is_err());
/// assert!(decode("%FF").is_err());
/// ```
pub fn decode(input: &str) -> Result<String, DecodeError> {
    if !input.contains('%') {
        return Ok(input.to_owned());
    }
    let bytes = decode_bytes(input.as_bytes())?;
    String::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8)
}

/// Decodes `%XX` escapes, keeping malformed escapes literally and replacing
/// invalid UTF-8 with U+FFFD.
pub fn decode_lossy(input: &str) -> String {
    if !input.contains('%') {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).copied().and_then(hex_value);
            let lo = bytes.get(i + 2).copied().and_then(hex_value);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decodes one `application/x-www-form-urlencoded` component: escapes
/// are decoded leniently and `+` means space.
pub fn decode_form(input: &str) -> String {
    let decoded = decode_lossy(input);
    if decoded.contains('+') {
        decoded.replace('+', " ")
    } else {
        decoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_encoding_keeps_only_unreserved() {
        assert_eq!(encode_component("AZaz09-._~"), "AZaz09-._~");
        assert_eq!(encode_component("a/b?c#d"), "a%2Fb%3Fc%23d");
        assert_eq!(encode_component("\u{e9}"), "%C3%A9");
        assert_eq!(encode_component(""), "");
    }

    #[test]
    fn path_encoding_keeps_segment_separators() {
        assert_eq!(
            encode("/v1/queues/a b", EncodeSet::Path),
            "/v1/queues/a%20b"
        );
        assert_eq!(encode("/x?y#z", EncodeSet::Path), "/x%3Fy%23z");
        assert_eq!(encode("/user@host:1", EncodeSet::Path), "/user@host:1");
    }

    #[test]
    fn query_value_encoding_escapes_structure() {
        assert_eq!(encode("a+b c", EncodeSet::QueryValue), "a%2Bb%20c");
        assert_eq!(encode("x/y?z", EncodeSet::QueryValue), "x/y?z");
    }

    #[test]
    fn strict_decode_reports_offsets() {
        assert_eq!(
            decode("ab%2"),
            Err(DecodeError::InvalidEscape { offset: 2 })
        );
        assert_eq!(decode("%zz"), Err(DecodeError::InvalidEscape { offset: 0 }));
        assert_eq!(decode("%c3%28"), Err(DecodeError::InvalidUtf8));
        assert_eq!(decode("%2f%2F").as_deref(), Ok("//"));
    }

    #[test]
    fn lossy_decode_keeps_bad_escapes() {
        assert_eq!(decode_lossy("100%"), "100%");
        assert_eq!(decode_lossy("%%41"), "%A");
        assert_eq!(decode_lossy("%FF"), "\u{fffd}");
    }

    #[test]
    fn encode_then_decode_round_trips() {
        let samples = [
            "",
            "simple",
            "sp ace",
            "slash/and?query",
            "\u{1f600} \u{e9}\u{e8}",
            "%41",
        ];
        for sample in samples {
            for set in [EncodeSet::Component, EncodeSet::Path, EncodeSet::QueryValue] {
                let encoded = encode(sample, set);
                assert_eq!(
                    decode(&encoded).as_deref(),
                    Ok(sample),
                    "{set:?} {sample:?}"
                );
            }
        }
    }
}
