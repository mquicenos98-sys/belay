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

/// Serializes with two-space indentation.
///
/// ```
/// use belay_json::json;
///
/// let v = json!({"queue": "emails", "attempts": [1, 2]});
/// assert_eq!(
///     belay_json::to_string_pretty(&v),
///     "{\n  \"queue\": \"emails\",\n  \"attempts\": [\n    1,\n    2\n  ]\n}"
/// );
/// ```
#[must_use]
pub fn to_string_pretty(value: &Value) -> String {
    to_string_with(value, &WriteOptions::pretty())
}

/// Serializes with explicit options.
#[must_use]
pub fn to_string_with(value: &Value, options: &WriteOptions) -> String {
    let mut out = String::with_capacity(128);
    Emitter::new(&mut out, options)
        .value(value)
        .expect("writing JSON into a String cannot fail");
    out
}

/// Writes compact JSON to an `io::Write` sink.
///
/// Output is buffered internally, so passing an unbuffered `File` or
/// `TcpStream` is fine. The sink is not flushed.
pub fn write_to<W: io::Write + ?Sized>(writer: &mut W, value: &Value) -> io::Result<()> {
    write_with(writer, value, &WriteOptions::compact())
}

/// Writes pretty JSON to an `io::Write` sink.
pub fn write_pretty_to<W: io::Write + ?Sized>(writer: &mut W, value: &Value) -> io::Result<()> {
    write_with(writer, value, &WriteOptions::pretty())
}

/// Writes JSON with explicit options to an `io::Write` sink.
pub fn write_with<W: io::Write + ?Sized>(
    writer: &mut W,
    value: &Value,
    options: &WriteOptions,
) -> io::Result<()> {
    let mut sink = IoSink {
        inner: writer,
        buf: Vec::with_capacity(IoSink::<W>::CHUNK),
        error: None,
    };
    let result = Emitter::new(&mut sink, options).value(value);
    match (result, sink.error.take()) {
        (_, Some(e)) => Err(e),
        (Err(_), None) => Err(io::Error::other("JSON formatter failed")),
        (Ok(()), None) => sink.drain(),
    }
}

/// Writes into a `fmt::Formatter`; backs `Display for Value`.
pub(crate) fn write_fmt(
    f: &mut fmt::Formatter<'_>,
    value: &Value,
    options: &WriteOptions,
) -> fmt::Result {
    Emitter::new(f, options).value(value)
}

/// Writes `s` as a quoted JSON string literal.
///
/// ```
/// let mut out = String::new();
/// belay_json::write_string(&mut out, "tab\there \"quoted\"").unwrap();
/// assert_eq!(out, r#""tab\there \"quoted\"""#);
/// ```
pub fn write_string<W: fmt::Write + ?Sized>(out: &mut W, s: &str) -> fmt::Result {
    write_escaped(out, s, false)
}

fn write_escaped<W: fmt::Write + ?Sized>(out: &mut W, s: &str, ascii_only: bool) -> fmt::Result {
    out.write_char('"')?;
    let bytes = s.as_bytes();
    let mut run = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let needs_escape = b < 0x20 || b == b'"' || b == b'\\' || (ascii_only && b >= 0x80);
        if !needs_escape {
            i += 1;
            continue;
        }
        out.write_str(&s[run..i])?;
        if b >= 0x80 {
            // `i` is at the lead byte of a multi-byte character because the
            // loop never stops inside one.
            let c = s[i..].chars().next().ok_or(fmt::Error)?;
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                write!(out, "\\u{:04x}", unit)?;
            }
            i += c.len_utf8();
        } else {
            match b {
                b'"' => out.write_str("\\\"")?,
                b'\\' => out.write_str("\\\\")?,
                b'\n' => out.write_str("\\n")?,
                b'\r' => out.write_str("\\r")?,
                b'\t' => out.write_str("\\t")?,
                0x08 => out.write_str("\\b")?,
                0x0c => out.write_str("\\f")?,
                _ => write!(out, "\\u{:04x}", b)?,
            }
            i += 1;
        }
        run = i;
    }
    out.write_str(&s[run..])?;
    out.write_char('"')
}

struct Emitter<'o, W: fmt::Write + ?Sized> {
    out: &'o mut W,
    options: &'o WriteOptions,
    depth: usize,
}

impl<'o, W: fmt::Write + ?Sized> Emitter<'o, W> {
    fn new(out: &'o mut W, options: &'o WriteOptions) -> Emitter<'o, W> {
        Emitter {
            out,
            options,
            depth: 0,
        }
    }

    fn value(&mut self, value: &Value) -> fmt::Result {
        match value {
            Value::Null => self.out.write_str("null"),
            Value::Bool(true) => self.out.write_str("true"),
            Value::Bool(false) => self.out.write_str("false"),
            Value::Number(n) => match n.as_u64() {
                Some(u) => write!(self.out, "{u}"),
                None => match n.as_i64() {
                    Some(i) => write!(self.out, "{i}"),
                    None => write_f64(self.out, n.as_f64()),
                },
            },
            Value::String(s) => write_escaped(self.out, s, self.options.ascii_only),
            Value::Array(items) => self.array(items),
            Value::Object(map) => self.object(map),
        }
    }

    fn array(&mut self, items: &[Value]) -> fmt::Result {
        if items.is_empty() {
            return self.out.write_str("[]");
        }
        self.out.write_char('[')?;
        self.depth += 1;
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.out.write_char(',')?;
            }
            self.newline()?;
            self.value(item)?;
        }
        self.depth -= 1;
        self.newline()?;
        self.out.write_char(']')
    }

    fn object(&mut self, map: &Map) -> fmt::Result {
        if map.is_empty() {
            return self.out.write_str("{}");
        }
        if self.options.sort_keys {
            let mut entries: Vec<(&str, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            self.members(entries.into_iter())
        } else {
            self.members(map.iter())
        }
    }

    fn members<'v>(&mut self, entries: impl Iterator<Item = (&'v str, &'v Value)>) -> fmt::Result {
        self.out.write_char('{')?;
        self.depth += 1;
        for (i, (key, value)) in entries.enumerate() {
            if i > 0 {
                self.out.write_char(',')?;
            }
            self.newline()?;
            write_escaped(self.out, key, self.options.ascii_only)?;
            self.out.write_str(if self.options.indent.is_some() {
                ": "
            } else {
                ":"
            })?;
            self.value(value)?;
        }
        self.depth -= 1;
        self.newline()?;
        self.out.write_char('}')
    }

    fn newline(&mut self) -> fmt::Result {
        const SPACES: &str = "                                ";
        let Some(width) = self.options.indent else {
            return Ok(());
        };
        self.out.write_char('\n')?;
        let mut remaining = width * self.depth;
        while remaining > 0 {
            let n = remaining.min(SPACES.len());
            self.out.write_str(&SPACES[..n])?;
            remaining -= n;
        }
        Ok(())
    }
}

/// Adapts `io::Write` to `fmt::Write`, buffering to avoid a syscall per
/// token and stashing the first I/O error (which `fmt::Error` cannot carry).
struct IoSink<'w, W: io::Write + ?Sized> {
    inner: &'w mut W,
    buf: Vec<u8>,
    error: Option<io::Error>,
}

impl<W: io::Write + ?Sized> IoSink<'_, W> {
    const CHUNK: usize = 8 * 1024;

    fn drain(&mut self) -> io::Result<()> {
        let result = self.inner.write_all(&self.buf);
        self.buf.clear();
        result
    }
}

impl<W: io::Write + ?Sized> fmt::Write for IoSink<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.buf.extend_from_slice(s.as_bytes());
        if self.buf.len() >= Self::CHUNK {
            if let Err(e) = self.drain() {
                self.error = Some(e);
                return Err(fmt::Error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    /// Fails after accepting `limit` bytes.
    struct FailingWriter {
        written: usize,
        limit: usize,
    }

    impl io::Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.written + buf.len() > self.limit {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "peer went away"));
            }
            self.written += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn escapes_quotes_backslashes_and_control_characters() {
        let v = json!("\"\\\u{0}\u{8}\u{c}\n\r\t\u{1f}\u{7f}/é");
        // DEL (U+007F) is not a JSON control character and passes through.
        let expected = "\"\\\"\\\\\\u0000\\b\\f\\n\\r\\t\\u001f\u{7f}/é\"";
        assert_eq!(to_string(&v), expected);
    }

    #[test]
    fn ascii_only_uses_surrogate_pairs() {
        let v = json!("é\u{1f600}\u{2028}");
        let out = to_string_with(&v, &WriteOptions::compact().ascii_only(true));
        assert_eq!(out, r#""\u00e9\ud83d\ude00\u2028""#);
        assert_eq!(crate::parse(&out).unwrap(), v);
    }

    #[test]
    fn integers_stay_integers_and_floats_stay_floats() {
        let v = json!([0, -1, u64::MAX, i64::MIN, 1.0, -0.0, 0.1, 1e300]);
        assert_eq!(
            to_string(&v),
            "[0,-1,18446744073709551615,-9223372036854775808,1.0,-0.0,0.1,1e300]"
        );
    }

    #[test]
    fn pretty_output_matches_two_space_layout() {
        let v = json!({"a": {}, "b": [], "c": [1, {"d": null}], "e": "x"});
        let expected = "{\n  \"a\": {},\n  \"b\": [],\n  \"c\": [\n    1,\n    {\n      \"d\": null\n    }\n  ],\n  \"e\": \"x\"\n}";
        assert_eq!(to_string_pretty(&v), expected);
        assert_eq!(to_string_pretty(&json!(5)), "5");
    }

    #[test]
    fn custom_indent_width() {
        let v = json!([1]);
        assert_eq!(
            to_string_with(&v, &WriteOptions::compact().indent(Some(4))),
            "[\n    1\n]"
        );
        assert_eq!(
            to_string_with(&v, &WriteOptions::compact().indent(Some(0))),
            "[\n1\n]"
        );
        let deep = json!([[[[[[[[[[[[[[[[[[1]]]]]]]]]]]]]]]]]]);
        let out = to_string_with(&deep, &WriteOptions::compact().indent(Some(3)));
        assert!(out.contains(&format!("\n{}1\n", " ".repeat(54))));
    }

    #[test]
    fn sort_keys_orders_nested_objects() {
        let v = json!({"z": {"y": 1, "x": 2}, "a": [ {"c": 1, "b": 2} ]});
        let out = to_string_with(&v, &WriteOptions::compact().sort_keys(true));
        assert_eq!(out, r#"{"a":[{"b":2,"c":1}],"z":{"x":2,"y":1}}"#);
    }

    #[test]
    fn io_output_matches_string_output() {
        let v = json!({"big": "x".repeat(20_000), "list": (0..2000).collect::<Vec<_>>()});
        let mut buf = Vec::new();
        write_to(&mut buf, &v).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), to_string(&v));
        let mut pretty = Vec::new();
        write_pretty_to(&mut pretty, &v).unwrap();
        assert_eq!(String::from_utf8(pretty).unwrap(), to_string_pretty(&v));
    }

    #[test]
    fn io_errors_are_propagated() {
        let v = json!({"big": "x".repeat(20_000)});
        let mut w = FailingWriter {
            written: 0,
            limit: 100,
        };
        let err = write_to(&mut w, &v).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        // Small documents fail on the final drain instead.
        let mut w = FailingWriter {
            written: 0,
            limit: 2,
        };
        assert!(write_to(&mut w, &json!([1, 2, 3])).is_err());
    }
}
