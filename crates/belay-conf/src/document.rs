//! [`Document`]: a parsed configuration together with its source text and
//! the location of every value, so that any later complaint about a value
//! can point at the line that set it.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use crate::convert::{ConvertError, ConvertErrorKind, FromValue};
use crate::error::{Error, Origin, Result};
use crate::parse::{self, with_article};
use crate::path::{KeyPath, Segment};
use crate::source::{LineIndex, Loc, Span};
use crate::value::{step_mut, Table, Value};

/// A parsed configuration file.
///
/// ```
/// use belay_conf::Document;
///
/// let doc = Document::parse("[server]\nlisten = \"0.0.0.0:7700\"\nthreads = 8\n")?;
/// assert_eq!(doc.get_str("server.listen")?, "0.0.0.0:7700");
/// assert_eq!(doc.get_int("server.threads")?, 8);
///
/// let err = doc.get_int("server.listen").unwrap_err();
/// assert_eq!(err.message(), "expected integer at server.listen, found string");
/// assert_eq!(err.line_col(), Some((2, 10)));
/// # Ok::<(), belay_conf::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct Document {
    root: Table,
    source: String,
    name: Option<String>,
    lines: LineIndex,
    locs: HashMap<KeyPath, Loc>,
}

impl Default for Document {
    fn default() -> Document {
        Document::new()
    }
}

impl Document {
    /// An empty document, for building configuration programmatically.
    pub fn new() -> Document {
        Document {
            root: Table::new(),
            source: String::new(),
            name: None,
            lines: LineIndex::new(""),
            locs: HashMap::new(),
        }
    }

    /// Parses configuration text.
    pub fn parse(src: &str) -> Result<Document> {
        Document::build(None, src)
    }

    /// Parses configuration text; `name` (usually a file name) appears in
    /// diagnostics as `name:line:column`.
    pub fn parse_named(name: &str, src: &str) -> Result<Document> {
        Document::build(Some(name), src)
    }

    /// Reads and parses a file. The file must be UTF-8.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Document> {
        let path = path.as_ref();
        let src = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        Document::build(Some(&path.display().to_string()), &src)
    }

    fn build(name: Option<&str>, src: &str) -> Result<Document> {
        let lines = LineIndex::new(src);
        let parsed = parse::parse_document(src).map_err(|d| d.into_error(src, &lines, name))?;
        Ok(Document {
            root: parsed.root,
            source: src.to_owned(),
            name: name.map(str::to_owned),
            lines,
            locs: parsed.locs,
        })
    }

    /// The name given at parse time, if any.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The text the document was parsed from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The root table.
    pub fn root(&self) -> &Table {
        &self.root
    }

    /// Consumes the document, keeping only its values.
    pub fn into_table(self) -> Table {
        self.root
    }

    /// The value at a dotted path such as `server.listen` or
    /// `schedules[0].cron`; `None` if absent or if `path` is malformed.
    pub fn get(&self, path: &str) -> Option<&Value> {
        self.root.lookup(&KeyPath::parse(path).ok()?)
    }

    /// The string at `path`; an error if absent or not a string.
    pub fn get_str(&self, path: &str) -> Result<&str> {
        self.typed(path, "string", Value::as_str)
    }

    /// The integer at `path`; an error if absent or not an integer.
    pub fn get_int(&self, path: &str) -> Result<i64> {
        self.typed(path, "integer", Value::as_integer)
    }

    /// The number at `path` as a float (integers are converted).
    pub fn get_float(&self, path: &str) -> Result<f64> {
        self.typed(path, "float", |v| f64::from_value(v).ok())
    }

    /// The boolean at `path`; an error if absent or not a boolean.
    pub fn get_bool(&self, path: &str) -> Result<bool> {
        self.typed(path, "boolean", Value::as_bool)
    }

    /// The array at `path`; an error if absent or not an array.
    pub fn get_array(&self, path: &str) -> Result<&[Value]> {
        self.typed(path, "array", Value::as_array)
    }

    /// The table at `path`; an error if absent or not a table.
    pub fn get_table(&self, path: &str) -> Result<&Table> {
        self.typed(path, "table", Value::as_table)
    }

    /// The value at `path` converted with [`FromValue`]; an error if absent
    /// or not convertible.
    pub fn get_as<T: FromValue>(&self, path: &str) -> Result<T> {
        let key = KeyPath::parse(path)?;
        let value = self.lookup(&key)?.ok_or_else(|| self.missing(&key))?;
        T::from_value(value).map_err(|e| self.convert_error(&key, e))
    }

    fn typed<'a, T>(
        &'a self,
        path: &str,
        expected: &'static str,
        convert: impl FnOnce(&'a Value) -> Option<T>,
    ) -> Result<T> {
        let key = KeyPath::parse(path)?;
        let value = self.lookup(&key)?.ok_or_else(|| self.missing(&key))?;
        convert(value).ok_or_else(|| self.mismatch(&key, expected, value.type_name()))
    }

    /// Where the value at `path` was set, if known.
    pub fn origin(&self, path: &str) -> Option<Origin> {
        self.origin_at(&KeyPath::parse(path).ok()?, false)
    }

    /// Sets the value at `path`, creating intermediate tables as needed, and
    /// returns the previous value. Array elements can be replaced but not
    /// appended. Values set this way have no recorded origin.
    pub fn set(&mut self, path: &str, value: impl Into<Value>) -> Result<Option<Value>> {
        let key = KeyPath::parse(path)?;
        self.set_at(&key, value.into(), None)
            .map_err(|message| Error::Path {
                path: path.to_owned(),
                message,
            })
    }

    /// Renders the document as TOML; see [`to_string`](crate::to_string).
    pub fn to_toml(&self) -> String {
        crate::ser::to_string(&self.root)
    }

    /// Looks up `path`, failing with a type error when an intermediate step
    /// is not a table or array (reading `server.listen` when `server = 5`).
    pub(crate) fn lookup(&self, path: &KeyPath) -> Result<Option<&Value>> {
        let mut walked = KeyPath::root();
        let mut current: Option<&Value> = None;
        for seg in path.segments() {
            let next = match (current, seg) {
                (None, Segment::Key(k)) => self.root.get(k),
                (Some(Value::Table(t)), Segment::Key(k)) => t.get(k),
                (Some(Value::Array(items)), Segment::Index(i)) => items.get(*i),
                (Some(v), Segment::Key(_)) => {
                    return Err(self.mismatch(&walked, "table", v.type_name()))
                }
                (Some(v), Segment::Index(_)) => {
                    return Err(self.mismatch(&walked, "array", v.type_name()))
                }
                (None, Segment::Index(_)) => {
                    return Err(Error::Path {
                        path: path.to_string(),
                        message: "a path cannot start with an index".into(),
                    })
                }
            };
            match next {
                Some(v) => current = Some(v),
                None => return Ok(None),
            }
            walked.push(seg.clone());
        }
        Ok(current)
    }

    /// Location of `path` or, failing that, of its nearest located ancestor
    /// (elements of an array set from the environment have no own location).
    /// `key` prefers the span of the key over that of the value.
    pub(crate) fn origin_at(&self, path: &KeyPath, key: bool) -> Option<Origin> {
        let mut p = path.clone();
        loop {
            match self.locs.get(&p) {
                Some(Loc::Env(var)) => return Some(Origin::Env(var.clone())),
                Some(Loc::Source { key: k, value }) => {
                    let span = if key { k.unwrap_or(*value) } else { *value };
                    return Some(Origin::Source(self.snippet(span)));
                }
                None => p = p.parent()?,
            }
        }
    }

    fn snippet(&self, span: Span) -> crate::error::Snippet {
        self.lines.snippet(&self.source, span, self.name.as_deref())
    }

    pub(crate) fn missing(&self, path: &KeyPath) -> Error {
        let table = path
            .parent()
            .filter(|p| !p.is_root() && self.locs.contains_key(p))
            .and_then(|p| self.origin_at(&p, false));
        Error::Missing {
            path: path.to_string(),
            table,
        }
    }

    pub(crate) fn mismatch(
        &self,
        path: &KeyPath,
        expected: &'static str,
        found: &'static str,
    ) -> Error {
        Error::Type {
            path: path.to_string(),
            expected,
            found,
            origin: self.origin_at(path, false),
        }
    }

    pub(crate) fn invalid(&self, path: &KeyPath, message: String) -> Error {
        Error::Invalid {
            path: path.to_string(),
            message,
            origin: self.origin_at(path, false),
        }
    }

    pub(crate) fn convert_error(&self, path: &KeyPath, err: ConvertError) -> Error {
        let full = path.join(err.path());
        match err.kind() {
            ConvertErrorKind::Type { expected, found } => self.mismatch(&full, *expected, *found),
            ConvertErrorKind::Invalid(message) => self.invalid(&full, message.clone()),
        }
    }

    /// Replaces or creates the value at `path`. `loc`, if given, becomes
    /// the location of the new value and of any table created on the way.
    pub(crate) fn set_at(
        &mut self,
        path: &KeyPath,
        value: Value,
        loc: Option<Loc>,
    ) -> std::result::Result<Option<Value>, String> {
        // Walking from a `Value` treats tables and arrays alike; the root
        // goes back into place whether or not the walk succeeds.
        let mut root = Value::Table(std::mem::take(&mut self.root));
        let mut created = Vec::new();
        let result = set_in(&mut root, path, value, &mut created);
        if let Value::Table(t) = root {
            self.root = t;
        }
        let previous = result?;
        if let Some(loc) = loc {
            for p in created {
                self.locs.insert(p, loc.clone());
            }
            self.locs.insert(path.clone(), loc);
        }
        Ok(previous)
    }
}

/// Walks `path` from `root`, creating missing tables (recorded in
/// `created`), and stores `value` at the end.
fn set_in(
    root: &mut Value,
    path: &KeyPath,
    value: Value,
    created: &mut Vec<KeyPath>,
) -> std::result::Result<Option<Value>, String> {
    let (last, parents) = path
        .segments()
        .split_last()
        .ok_or("cannot replace the root table")?;
    let mut walked = KeyPath::root();
    let mut cur = root;
    for seg in parents {
        if let (Value::Table(t), Segment::Key(k)) = (&mut *cur, seg) {
            if !t.contains_key(k) {
                t.insert(k.clone(), Table::new());
                created.push(walked.key(k));
            }
        }
        let (ty, len) = (cur.type_name(), cur.as_array().map_or(0, <[Value]>::len));
        cur = match step_mut(cur, seg) {
            Some(next) => next,
            None => return Err(no_slot(&walked, seg, ty, len)),
        };
        walked.push(seg.clone());
    }
    let (ty, len) = (cur.type_name(), cur.as_array().map_or(0, <[Value]>::len));
    match (cur, last) {
        (Value::Table(t), Segment::Key(k)) => Ok(t.insert(k.clone(), value)),
        (Value::Array(items), Segment::Index(i)) if *i < len => {
            Ok(Some(std::mem::replace(&mut items[*i], value)))
        }
        _ => Err(no_slot(&walked, last, ty, len)),
    }
}

fn no_slot(parent: &KeyPath, seg: &Segment, type_name: &str, len: usize) -> String {
    match seg {
        Segment::Index(i) if type_name == "array" => {
            let plural = if len == 1 { "" } else { "s" };
            format!("index {i} is out of bounds; `{parent}` has {len} element{plural}")
        }
        Segment::Index(_) => format!("`{parent}` is {}, not an array", with_article(type_name)),
        Segment::Key(_) => format!("`{parent}` is {}, not a table", with_article(type_name)),
    }
}
