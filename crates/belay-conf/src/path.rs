//! Key paths address values inside a document: `server.listen`,
//! `queues."eu.west".max_concurrency`, `schedules[2].cron`.
//!
//! Keys use TOML key syntax (bare, `"basic"` or `'literal'`) and array
//! elements are selected with `[n]`. Diagnostics print paths in the same
//! syntax, so a path from an error message can be pasted back into a lookup.

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::parse::lexer::{is_bare_key_char, Cursor};

/// One step of a [`KeyPath`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Segment {
    /// A key within a table.
    Key(String),
    /// An element of an array.
    Index(usize),
}

/// A sequence of keys and array indices, relative to some table.
///
/// ```
/// use belay_conf::{KeyPath, Segment};
///
/// let p: KeyPath = r#"queues."eu.west"[0].limit"#.parse().unwrap();
/// assert_eq!(p.segments()[1], Segment::Key("eu.west".into()));
/// assert_eq!(p.to_string(), r#"queues."eu.west"[0].limit"#);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyPath {
    segments: Vec<Segment>,
}

impl KeyPath {
    /// The empty path, naming the root table.
    pub fn root() -> KeyPath {
        KeyPath::default()
    }

    /// Parses the textual form. The empty string is the root path.
    pub fn parse(text: &str) -> Result<KeyPath> {
        let fail = |message: String| Error::Path {
            path: text.to_owned(),
            message,
        };
        let mut path = KeyPath::root();
        if text.is_empty() {
            return Ok(path);
        }
        let mut cur = Cursor::new(text);
        let mut want_key = true;
        loop {
            if want_key {
                let seg = cur.simple_key().map_err(|d| fail(d.message))?;
                path.segments.push(Segment::Key(seg.name));
                want_key = false;
                continue;
            }
            let at = cur.pos() + 1;
            match cur.bump() {
                None => return Ok(path),
                Some('.') => want_key = true,
                Some('[') => {
                    let digits: String = std::iter::from_fn(|| {
                        let c = cur.peek().filter(char::is_ascii_digit)?;
                        cur.bump();
                        Some(c)
                    })
                    .collect();
                    let index = digits.parse().ok().filter(|_| cur.eat(']'));
                    let index =
                        index.ok_or_else(|| fail(format!("malformed index at position {at}")))?;
                    path.segments.push(Segment::Index(index));
                }
                Some(c) => return Err(fail(format!("unexpected `{c}` at position {at}"))),
            }
        }
    }

    /// The individual steps.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Whether this is the root path.
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// This path extended by a key.
    #[must_use]
    pub fn key(&self, key: &str) -> KeyPath {
        let mut p = self.clone();
        p.segments.push(Segment::Key(key.to_owned()));
        p
    }

    /// This path extended by an array index.
    #[must_use]
    pub fn index(&self, index: usize) -> KeyPath {
        let mut p = self.clone();
        p.segments.push(Segment::Index(index));
        p
    }

    /// This path followed by `other`.
    #[must_use]
    pub fn join(&self, other: &KeyPath) -> KeyPath {
        let mut p = self.clone();
        p.segments.extend(other.segments.iter().cloned());
        p
    }

    /// The path without its last segment; `None` for the root.
    pub fn parent(&self) -> Option<KeyPath> {
        let (_, init) = self.segments.split_last()?;
        Some(KeyPath {
            segments: init.to_vec(),
        })
    }

    /// The last segment, if any.
    pub fn last(&self) -> Option<&Segment> {
        self.segments.last()
    }

    /// Whether `prefix` is this path or one of its ancestors.
    pub fn starts_with(&self, prefix: &KeyPath) -> bool {
        self.segments.starts_with(&prefix.segments)
    }

    pub(crate) fn push(&mut self, segment: Segment) {
        self.segments.push(segment);
    }
}

impl<'a> FromIterator<&'a str> for KeyPath {
    fn from_iter<I: IntoIterator<Item = &'a str>>(keys: I) -> KeyPath {
        KeyPath {
            segments: keys
                .into_iter()
                .map(|k| Segment::Key(k.to_owned()))
                .collect(),
        }
    }
}

impl FromStr for KeyPath {
    type Err = Error;

    fn from_str(s: &str) -> Result<KeyPath> {
        KeyPath::parse(s)
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.segments.iter().enumerate() {
            match seg {
                Segment::Key(k) => {
                    if i > 0 {
                        f.write_str(".")?;
                    }
                    let mut quoted = String::new();
                    write_key(&mut quoted, k);
                    f.write_str(&quoted)?;
                }
                Segment::Index(n) => write!(f, "[{n}]")?,
            }
        }
        Ok(())
    }
}

/// Appends `key` bare when TOML allows it, quoted otherwise.
pub(crate) fn write_key(out: &mut String, key: &str) {
    if !key.is_empty() && key.chars().all(is_bare_key_char) {
        out.push_str(key);
    } else {
        write_basic_string(out, key);
    }
}

/// Appends `s` as a TOML basic string with every mandatory escape applied.
pub(crate) fn write_basic_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c < ' ' || c == '\u{7f}' => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(path: &KeyPath) -> Vec<String> {
        path.segments()
            .iter()
            .map(|s| match s {
                Segment::Key(k) => k.clone(),
                Segment::Index(i) => format!("#{i}"),
            })
            .collect()
    }

    #[test]
    fn parses_keys_indices_and_quoted_segments() {
        let p = KeyPath::parse(r#"schedules[12].template."meta.team"[0]"#).unwrap();
        assert_eq!(
            keys(&p),
            ["schedules", "#12", "template", "meta.team", "#0"]
        );
        let p = KeyPath::parse("a.'b c'.\"\\u0041\"").unwrap();
        assert_eq!(keys(&p), ["a", "b c", "A"]);
        assert!(KeyPath::parse("").unwrap().is_root());
    }

    #[test]
    fn display_round_trips_through_parse() {
        let p: KeyPath = ["queues", "eu.west", "", "tab\there"].into_iter().collect();
        let text = p.to_string();
        assert_eq!(text, r#"queues."eu.west".""."tab\there""#);
        assert_eq!(KeyPath::parse(&text).unwrap(), p);
    }

    #[test]
    fn rejects_malformed_paths() {
        let cases = [
            ("a..b", "expected a key, found `.`"),
            ("a.", "expected a key, found end of file"),
            ("a[x]", "malformed index at position 2"),
            ("a[1", "malformed index at position 2"),
            ("a b", "unexpected ` ` at position 2"),
            ("[0]", "expected a key, found `[`"),
        ];
        for (text, message) in cases {
            match KeyPath::parse(text) {
                Err(Error::Path { message: m, .. }) => assert_eq!(m, message, "{text}"),
                other => panic!("{text}: unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn navigation_helpers() {
        let p = KeyPath::root().key("a").index(2).key("b");
        assert_eq!(p.to_string(), "a[2].b");
        assert_eq!(p.parent().unwrap().to_string(), "a[2]");
        assert!(p.starts_with(&KeyPath::root().key("a")));
        assert!(!p.starts_with(&KeyPath::root().key("b")));
        assert_eq!(p.last(), Some(&Segment::Key("b".into())));
        assert_eq!(KeyPath::root().parent(), None);
    }
}
