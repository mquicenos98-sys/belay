//! RFC 6901 JSON Pointers (`/steps/0/result`).
//!
//! [`Value::pointer`](crate::Value::pointer) resolves a pointer string on the
//! fly without allocating unless a token contains an escape. [`Pointer`] is
//! the parsed form, for pointers that are validated once (e.g. workflow
//! templates at submit time) and resolved many times.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use crate::Value;

/// A parsed JSON Pointer: a sequence of unescaped reference tokens.
///
/// ```
/// use belay_json::{json, Pointer};
///
/// let p: Pointer = "/steps/fetch/result/url".parse().unwrap();
/// let doc = json!({"steps": {"fetch": {"result": {"url": "http://x"}}}});
/// assert_eq!(p.get(&doc), Some(&json!("http://x")));
///
/// let built = Pointer::root().child("a/b").child(0);
/// assert_eq!(built.to_string(), "/a~1b/0");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Pointer {
    tokens: Vec<String>,
}

/// A string that is not a valid JSON Pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerError {
    offset: usize,
    reason: &'static str,
}

impl PointerError {
    /// Byte offset of the problem within the pointer string.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl fmt::Display for PointerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid JSON pointer at offset {}: {}",
            self.offset, self.reason
        )
    }
}

impl std::error::Error for PointerError {}

impl Pointer {
    /// The empty pointer, which refers to the whole document.
    #[must_use]
    pub fn root() -> Pointer {
        Pointer::default()
    }

    /// Parses `""` or `/token/token...`, decoding `~1` to `/` and `~0` to `~`.
    pub fn parse(s: &str) -> Result<Pointer, PointerError> {
        if s.is_empty() {
            return Ok(Pointer::root());
        }
        let Some(rest) = s.strip_prefix('/') else {
            return Err(PointerError {
                offset: 0,
                reason: "must be empty or start with '/'",
            });
        };
        let mut tokens = Vec::new();
        let mut offset = 1;
        for raw in rest.split('/') {
            let token = unescape(raw).map_err(|at| PointerError {
                offset: offset + at,
                reason: "'~' must be followed by '0' or '1'",
            })?;
            tokens.push(token.into_owned());
            offset += raw.len() + 1;
        }
        Ok(Pointer { tokens })
    }

    /// True for the empty pointer.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The unescaped reference tokens.
    pub fn tokens(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.tokens.iter().map(String::as_str)
    }

    /// Appends a token: an object key, or an array position in decimal.
    pub fn push(&mut self, token: impl fmt::Display) {
        self.tokens.push(token.to_string());
    }

    /// Returns a copy with `token` appended.
    #[must_use]
    pub fn child(&self, token: impl fmt::Display) -> Pointer {
        let mut child = self.clone();
        child.push(token);
        child
    }

    /// The pointer to the containing value, or `None` at the root.
    #[must_use]
    pub fn parent(&self) -> Option<Pointer> {
        let (_, parent) = self.tokens.split_last()?;
        Some(Pointer {
            tokens: parent.to_vec(),
        })
    }

    /// Resolves against `value`.
    #[must_use]
    pub fn get<'v>(&self, value: &'v Value) -> Option<&'v Value> {
        self.tokens.iter().try_fold(value, |v, t| step(v, t))
    }

    /// Resolves against `value`, mutably.
    pub fn get_mut<'v>(&self, value: &'v mut Value) -> Option<&'v mut Value> {
        self.tokens.iter().try_fold(value, |v, t| step_mut(v, t))
    }
}

impl fmt::Display for Pointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for token in &self.tokens {
            f.write_str("/")?;
            f.write_str(&escape_token(token))?;
        }
        Ok(())
    }
}

impl FromStr for Pointer {
    type Err = PointerError;

    fn from_str(s: &str) -> Result<Pointer, PointerError> {
        Pointer::parse(s)
    }
}

/// Escapes one reference token (`~` to `~0`, `/` to `~1`).
#[must_use]
pub fn escape_token(token: &str) -> Cow<'_, str> {
    if token.contains(['~', '/']) {
        Cow::Owned(token.replace('~', "~0").replace('/', "~1"))
    } else {
        Cow::Borrowed(token)
    }
}

/// Decodes `~0`/`~1`; on a bad escape returns the byte offset of the `~`.
fn unescape(raw: &str) -> Result<Cow<'_, str>, usize> {
    if !raw.contains('~') {
        return Ok(Cow::Borrowed(raw));
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.char_indices();
    while let Some((i, c)) = chars.next() {
        if c != '~' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some((_, '0')) => out.push('~'),
            Some((_, '1')) => out.push('/'),
            _ => return Err(i),
        }
    }
    Ok(Cow::Owned(out))
}

/// Parses an array index token. `-` (the position after the last
/// element) never resolves.
fn array_index(token: &str) -> Option<usize> {
    token.parse().ok()
}

fn step<'v>(value: &'v Value, token: &str) -> Option<&'v Value> {
    match value {
        Value::Object(map) => map.get(token),
        Value::Array(items) => items.get(array_index(token)?),
        _ => None,
    }
}

fn step_mut<'v>(value: &'v mut Value, token: &str) -> Option<&'v mut Value> {
    match value {
        Value::Object(map) => map.get_mut(token),
        Value::Array(items) => items.get_mut(array_index(token)?),
        _ => None,
    }
}

pub(crate) fn resolve<'v>(value: &'v Value, pointer: &str) -> Option<&'v Value> {
    if pointer.is_empty() {
        return Some(value);
    }
    pointer
        .strip_prefix('/')?
        .split('/')
        .try_fold(value, |v, raw| step(v, &unescape(raw).ok()?))
}

pub(crate) fn resolve_mut<'v>(value: &'v mut Value, pointer: &str) -> Option<&'v mut Value> {
    if pointer.is_empty() {
        return Some(value);
    }
    pointer
        .strip_prefix('/')?
        .split('/')
        .try_fold(value, |v, raw| step_mut(v, &unescape(raw).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    /// The example document from RFC 6901 §5.
    fn rfc_doc() -> Value {
        json!({
            "foo": ["bar", "baz"],
            "": 0,
            "a/b": 1,
            "c%d": 2,
            "e^f": 3,
            "g|h": 4,
            "i\\j": 5,
            "k\"l": 6,
            " ": 7,
            "m~n": 8
        })
    }

    #[test]
    fn resolves_every_rfc_6901_example() {
        let doc = rfc_doc();
        let cases: [(&str, Value); 12] = [
            ("", doc.clone()),
            ("/foo", json!(["bar", "baz"])),
            ("/foo/0", json!("bar")),
            ("/", json!(0)),
            ("/a~1b", json!(1)),
            ("/c%d", json!(2)),
            ("/e^f", json!(3)),
            ("/g|h", json!(4)),
            ("/i\\j", json!(5)),
            ("/k\"l", json!(6)),
            ("/ ", json!(7)),
            ("/m~0n", json!(8)),
        ];
        for (pointer, expected) in cases {
            assert_eq!(doc.pointer(pointer), Some(&expected), "{pointer:?}");
            let parsed = Pointer::parse(pointer).unwrap();
            assert_eq!(parsed.get(&doc), Some(&expected), "{pointer:?}");
            assert_eq!(parsed.to_string(), pointer, "display round-trips");
        }
    }

    #[test]
    fn malformed_pointers_resolve_to_none_or_error() {
        let doc = rfc_doc();
        assert_eq!(doc.pointer("foo"), None);
        assert_eq!(doc.pointer("/m~2n"), None);
        assert_eq!(doc.pointer("/foo/0/deeper"), None);
        let err = Pointer::parse("foo").unwrap_err();
        assert_eq!(err.offset(), 0);
        let err = Pointer::parse("/ok/bad~").unwrap_err();
        assert_eq!(err.offset(), 7);
        assert_eq!(
            err.to_string(),
            "invalid JSON pointer at offset 7: '~' must be followed by '0' or '1'"
        );
    }

    #[test]
    fn mutable_resolution_edits_in_place() {
        let mut doc = json!({"steps": [{"state": "pending"}]});
        *doc.pointer_mut("/steps/0/state").unwrap() = json!("done");
        assert_eq!(doc["steps"][0]["state"], "done");
        let p = Pointer::root().child("steps").child(0);
        p.get_mut(&mut doc).unwrap()["attempt"] = json!(2);
        assert_eq!(doc.pointer("/steps/0/attempt"), Some(&json!(2)));
        assert!(doc.pointer_mut("/steps/1").is_none());
    }

    #[test]
    fn parent_and_tokens() {
        let p = Pointer::parse("/a~1b/c~0d/0").unwrap();
        assert_eq!(p.tokens().collect::<Vec<_>>(), ["a/b", "c~d", "0"]);
        assert_eq!(p.parent().unwrap().to_string(), "/a~1b/c~0d");
        assert!(Pointer::root().parent().is_none());
        assert!(Pointer::root().is_root());
        assert_eq!(escape_token("plain"), Cow::Borrowed("plain"));
    }
}
