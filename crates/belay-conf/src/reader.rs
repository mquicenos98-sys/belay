//! Typed extraction with unused-key tracking.
//!
//! A [`Reader`] hands out values and records every key the consumer asks
//! for, present or not. Afterwards [`Reader::unused_keys`] lists what the
//! document contains that nobody asked for (almost always a typo), each with
//! the closest requested key as a "did you mean" suggestion.
//!
//! Sub-readers returned by [`Reader::table`], [`Reader::tables`] and
//! [`Reader::array_of_tables`] resolve paths relative to their table and
//! share the tracking of the reader they came from.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fmt::Display;
use std::rc::Rc;
use std::str::FromStr;

use crate::convert::{ConvertError, FromValue};
use crate::error::{Error, Result, UnusedKey};
use crate::path::{KeyPath, Segment};
use crate::suggest;
use crate::value::{Table, Value};
use crate::Document;

#[derive(Debug, Default)]
struct Tracking {
    /// Paths whose whole subtree has been read.
    consumed: HashSet<KeyPath>,
    /// Containers the consumer looked inside; their children are checked
    /// one by one, while an unentered container is reported as a whole.
    entered: HashSet<KeyPath>,
    /// Key names requested under each container, whether present or not.
    /// These are the candidates for "did you mean".
    requested: HashMap<KeyPath, BTreeSet<String>>,
}

/// Reads typed values out of a [`Document`] and remembers what was read.
///
/// ```
/// use belay_conf::{Document, Reader};
///
/// let doc = Document::parse(r#"
///     [server]
///     listen = "0.0.0.0:7700"
///     treads = 8
///
///     [queues.emails]
///     max_concurrency = 4
/// "#)?;
/// let r = Reader::new(&doc);
/// let listen = r.string_or("server.listen", "127.0.0.1:7700")?;
/// let threads: Option<u32> = r.opt("server.threads")?;
/// let mut queues = Vec::new();
/// for (name, q) in r.tables("queues")? {
///     queues.push((name, q.or("max_concurrency", 1u32)?));
/// }
/// assert_eq!((listen.as_str(), threads), ("0.0.0.0:7700", None));
/// assert_eq!(queues, [("emails".to_string(), 4)]);
///
/// let unused = r.unused_keys();
/// assert_eq!(unused[0].path, "server.treads");
/// assert_eq!(unused[0].suggestion.as_deref(), Some("server.threads"));
/// # Ok::<(), belay_conf::Error>(())
/// ```
#[derive(Clone)]
pub struct Reader<'a> {
    doc: &'a Document,
    base: KeyPath,
    track: Rc<RefCell<Tracking>>,
}

impl fmt::Debug for Reader<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader")
            .field("path", &self.base.to_string())
            .finish()
    }
}

impl<'a> Reader<'a> {
    /// A reader over the whole document.
    pub fn new(doc: &'a Document) -> Reader<'a> {
        Reader {
            doc,
            base: KeyPath::root(),
            track: Rc::default(),
        }
    }

    /// The document being read.
    pub fn document(&self) -> &'a Document {
        self.doc
    }

    /// Absolute path of the table this reader is scoped to (root: empty).
    pub fn path(&self) -> &KeyPath {
        &self.base
    }

    /// The required value at `path`, converted with [`FromValue`].
    pub fn req<T: FromValue>(&self, path: &str) -> Result<T> {
        let abs = self.resolve(path)?;
        self.convert(&abs, T::from_value)?
            .ok_or_else(|| self.doc.missing(&abs))
    }

    /// The value at `path` if present.
    pub fn opt<T: FromValue>(&self, path: &str) -> Result<Option<T>> {
        self.opt_with(path, T::from_value)
    }

    /// The value at `path`, or `default` if absent. A present value of the
    /// wrong type is still an error.
    pub fn or<T: FromValue>(&self, path: &str, default: T) -> Result<T> {
        Ok(self.opt(path)?.unwrap_or(default))
    }

    /// The required string at `path`.
    pub fn string(&self, path: &str) -> Result<String> {
        self.req(path)
    }

    /// The string at `path`, if present.
    pub fn opt_string(&self, path: &str) -> Result<Option<String>> {
        self.opt(path)
    }

    /// The string at `path`, or `default`.
    pub fn string_or(&self, path: &str, default: &str) -> Result<String> {
        Ok(self.opt(path)?.unwrap_or_else(|| default.to_owned()))
    }

    /// The integer at `path`, if present.
    pub fn opt_int(&self, path: &str) -> Result<Option<i64>> {
        self.opt(path)
    }

    /// The required value at `path`, parsed from text with [`FromStr`].
    ///
    /// This is the hook for domain types the configuration stores as text:
    /// durations, socket addresses, sync policies. Strings and unquoted
    /// date-times are parsed as written; integers and floats through their
    /// decimal text, so `timeout = 30` reaches the parser as `"30"`. A parse
    /// failure becomes [`Error::Invalid`] with the key path and location.
    ///
    /// ```
    /// use std::net::SocketAddr;
    /// use belay_conf::{Document, Reader};
    ///
    /// let doc = Document::parse("listen = \"0.0.0.0:77000\"")?;
    /// let err = Reader::new(&doc).parsed::<SocketAddr>("listen").unwrap_err();
    /// assert_eq!(err.message(), "invalid value for listen: invalid socket address syntax");
    /// assert_eq!(err.line_col(), Some((1, 10)));
    /// # Ok::<(), belay_conf::Error>(())
    /// ```
    pub fn parsed<T>(&self, path: &str) -> Result<T>
    where
        T: FromStr,
        T::Err: Display,
    {
        let abs = self.resolve(path)?;
        self.convert(&abs, parse_text::<T>)?
            .ok_or_else(|| self.doc.missing(&abs))
    }

    /// Like [`Reader::parsed`], for an optional key.
    pub fn opt_parsed<T>(&self, path: &str) -> Result<Option<T>>
    where
        T: FromStr,
        T::Err: Display,
    {
        self.opt_with(path, parse_text::<T>)
    }

    /// The value at `path`, if present, converted by `f`. Errors from `f`
    /// are reported with the key path and location:
    ///
    /// ```
    /// use belay_conf::{ConvertError, Document, Reader, Value};
    ///
    /// let doc = Document::parse("[storage]\nsync = \"sometimes\"")?;
    /// let r = Reader::new(&doc);
    /// let err = r
    ///     .opt_with("storage.sync", |v: &Value| match v.as_str() {
    ///         Some("always" | "never") => Ok(v.clone()),
    ///         Some(other) => Err(format!("unknown sync policy `{other}`").into()),
    ///         None => Err(ConvertError::mismatch("string", v)),
    ///     })
    ///     .unwrap_err();
    /// assert_eq!(err.message(), "invalid value for storage.sync: unknown sync policy `sometimes`");
    /// # Ok::<(), belay_conf::Error>(())
    /// ```
    pub fn opt_with<T, F>(&self, path: &str, f: F) -> Result<Option<T>>
    where
        F: FnOnce(&'a Value) -> std::result::Result<T, ConvertError>,
    {
        let abs = self.resolve(path)?;
        self.convert(&abs, f)
    }

    /// The raw value at `path`, marking all of it as read.
    pub fn value(&self, path: &str) -> Result<Option<&'a Value>> {
        self.opt_with(path, Ok)
    }

    /// A reader scoped to the table at `path`, if present.
    pub fn table(&self, path: &str) -> Result<Option<Reader<'a>>> {
        let abs = self.resolve(path)?;
        match self.fetch(&abs)? {
            None => Ok(None),
            Some(Value::Table(_)) => Ok(Some(self.scoped(abs))),
            Some(other) => Err(self.doc.mismatch(&abs, "table", other.type_name())),
        }
    }

    /// Every entry of the table at `path` as `(name, reader)`, in document
    /// order; for maps of named sections such as `[queues.<name>]`. Absent
    /// means empty; an entry that is not a table is an error.
    pub fn tables(&self, path: &str) -> Result<Vec<(String, Reader<'a>)>> {
        let Some(parent) = self.table(path)? else {
            return Ok(Vec::new());
        };
        let table = self.table_at(&parent.base);
        table
            .iter()
            .map(|(name, value)| {
                let abs = parent.base.key(name);
                self.request(&abs);
                match value {
                    Value::Table(_) => Ok((name.to_owned(), self.scoped(abs))),
                    other => Err(self.doc.mismatch(&abs, "table", other.type_name())),
                }
            })
            .collect()
    }

    /// A reader per element of the array of tables at `path`: `[[schedules]]`
    /// sections or an inline `[{ ... }, { ... }]`. Absent means empty.
    pub fn array_of_tables(&self, path: &str) -> Result<Vec<Reader<'a>>> {
        let abs = self.resolve(path)?;
        let items = match self.fetch(&abs)? {
            None => return Ok(Vec::new()),
            Some(Value::Array(items)) => items,
            Some(other) => {
                return Err(self
                    .doc
                    .mismatch(&abs, "array of tables", other.type_name()))
            }
        };
        self.enter(&abs);
        items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let element = abs.index(i);
                match item {
                    Value::Table(_) => Ok(self.scoped(element)),
                    other => Err(self.doc.mismatch(&element, "table", other.type_name())),
                }
            })
            .collect()
    }

    /// Keys of this reader's table, in document order. Listing keys does
    /// not count as reading them.
    pub fn keys(&self) -> Vec<String> {
        self.table_at(&self.base)
            .keys()
            .map(str::to_owned)
            .collect()
    }

    /// An [`Error::Invalid`] for the value at `path`, located in the source,
    /// for validation done by the consumer after extraction:
    ///
    /// ```
    /// # use belay_conf::{Document, Reader};
    /// # let doc = Document::parse("[server]\nthreads = 0")?;
    /// let r = Reader::new(&doc);
    /// let threads: u32 = r.or("server.threads", 4)?;
    /// if threads == 0 {
    ///     let err = r.invalid("server.threads", "must be at least 1");
    ///     assert_eq!(err.line_col(), Some((2, 11)));
    /// }
    /// # Ok::<(), belay_conf::Error>(())
    /// ```
    pub fn invalid(&self, path: &str, message: impl Into<String>) -> Error {
        match self.resolve(path) {
            Ok(abs) => self.doc.invalid(&abs, message.into()),
            Err(e) => e,
        }
    }

    /// Keys in the document that were never read, in document order. A
    /// table nobody looked into is reported once rather than key by key.
    /// Covers the whole document regardless of which reader it is called on.
    pub fn unused_keys(&self) -> Vec<UnusedKey> {
        let track = self.track.borrow();
        let mut out = Vec::new();
        self.walk_table(&track, &KeyPath::root(), self.doc.root(), &mut out);
        out
    }

    /// Fails with [`Error::Unknown`] for the first unused key, for callers
    /// that treat unknown keys as errors rather than warnings.
    pub fn deny_unused(&self) -> Result<()> {
        match self.unused_keys().into_iter().next() {
            Some(unused) => Err(Error::Unknown(unused)),
            None => Ok(()),
        }
    }

    fn resolve(&self, path: &str) -> Result<KeyPath> {
        Ok(self.base.join(&KeyPath::parse(path)?))
    }

    fn scoped(&self, abs: KeyPath) -> Reader<'a> {
        self.enter(&abs);
        Reader {
            doc: self.doc,
            base: abs,
            track: Rc::clone(&self.track),
        }
    }

    fn table_at(&self, abs: &KeyPath) -> &'a Table {
        if abs.is_root() {
            return self.doc.root();
        }
        self.doc
            .root()
            .lookup(abs)
            .and_then(Value::as_table)
            .expect("readers are only scoped to tables of their document")
    }

    fn fetch(&self, abs: &KeyPath) -> Result<Option<&'a Value>> {
        self.request(abs);
        self.doc.lookup(abs)
    }

    fn convert<T, F>(&self, abs: &KeyPath, f: F) -> Result<Option<T>>
    where
        F: FnOnce(&'a Value) -> std::result::Result<T, ConvertError>,
    {
        let Some(value) = self.fetch(abs)? else {
            return Ok(None);
        };
        self.track.borrow_mut().consumed.insert(abs.clone());
        f(value)
            .map(Some)
            .map_err(|e| self.doc.convert_error(abs, e))
    }

    /// Records a request for `abs`: every ancestor counts as entered and
    /// each key name is noted as known in its parent.
    fn request(&self, abs: &KeyPath) {
        let mut track = self.track.borrow_mut();
        let mut prefix = KeyPath::root();
        for seg in abs.segments() {
            track.entered.insert(prefix.clone());
            if let Segment::Key(k) = seg {
                track
                    .requested
                    .entry(prefix.clone())
                    .or_default()
                    .insert(k.clone());
            }
            prefix.push(seg.clone());
        }
    }

    fn enter(&self, abs: &KeyPath) {
        self.track.borrow_mut().entered.insert(abs.clone());
    }

    fn walk_table(&self, t: &Tracking, path: &KeyPath, table: &Table, out: &mut Vec<UnusedKey>) {
        for (name, value) in table {
            self.walk_value(t, path.key(name), value, out);
        }
    }

    fn walk_value(&self, t: &Tracking, path: KeyPath, value: &Value, out: &mut Vec<UnusedKey>) {
        if t.consumed.contains(&path) {
            return;
        }
        if t.entered.contains(&path) {
            match value {
                Value::Table(table) => return self.walk_table(t, &path, table, out),
                Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        self.walk_value(t, path.index(i), item, out);
                    }
                    return;
                }
                _ => {}
            }
        }
        let suggestion = match (path.parent(), path.last()) {
            (Some(parent), Some(Segment::Key(name))) => {
                t.requested.get(&parent).and_then(|known| {
                    suggest::closest(name, known.iter().map(String::as_str))
                        .map(|s| parent.key(s).to_string())
                })
            }
            _ => None,
        };
        out.push(UnusedKey {
            path: path.to_string(),
            origin: self.doc.origin_at(&path, true),
            suggestion,
        });
    }
}

fn parse_text<T>(value: &Value) -> std::result::Result<T, ConvertError>
where
    T: FromStr,
    T::Err: Display,
{
    let text = match value {
        Value::String(s) | Value::Datetime(s) => s.clone(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        other => return Err(ConvertError::mismatch("string", other)),
    };
    text.parse()
        .map_err(|e: T::Err| ConvertError::invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::net::IpAddr;

    fn doc(src: &str) -> Document {
        Document::parse(src).unwrap()
    }

    fn unused(r: &Reader<'_>) -> Vec<(String, Option<String>)> {
        let unused = r.unused_keys().into_iter();
        unused.map(|u| (u.path, u.suggestion)).collect()
    }

    #[test]
    fn untouched_tables_are_reported_once_with_a_suggestion() {
        let d = doc("[sever]\nlisten = 'x'\nthreads = 2\n[server]\nthreads = 4\n");
        let r = Reader::new(&d);
        assert_eq!(r.opt_string("server.listen").unwrap(), None);
        assert_eq!(r.or("server.threads", 1u32).unwrap(), 4);
        let expected = [("sever".to_owned(), Some("server".to_owned()))];
        assert_eq!(unused(&r), expected);
        let text = r.unused_keys()[0].to_string();
        assert!(text.contains("1 | [sever]\n  |  ^^^^^"), "{text}");
    }

    #[test]
    fn whole_values_count_as_read() {
        let d = doc("[cluster]\nseeds = ['a']\nmeta = { zone = 'eu', rack = '4' }\nextra = 1\n");
        let r = Reader::new(&d);
        let seeds: Vec<String> = r.req("cluster.seeds").unwrap();
        let meta: BTreeMap<String, String> = r.req("cluster.meta").unwrap();
        assert_eq!((seeds.len(), meta.len()), (1, 2));
        assert_eq!(unused(&r), [("cluster.extra".to_owned(), None)]);
        assert!(r.value("cluster.extra").unwrap().is_some());
        r.deny_unused().unwrap();
    }

    #[test]
    fn type_errors_carry_path_and_location() {
        let d = doc("[server]\nthreads = \"8\"\nports = [1, 'x']\n");
        let r = Reader::new(&d);
        let errors = [
            r.opt_int("server.threads").unwrap_err(),
            r.req::<Vec<u16>>("server.ports").unwrap_err(),
            r.tables("server").unwrap_err(),
            r.req::<String>("server.threads.x").unwrap_err(),
        ];
        let got: Vec<_> = errors.iter().map(|e| (e.message(), e.line_col())).collect();
        let at_threads = Some((2, 11));
        assert_eq!(
            got,
            [
                (
                    "expected integer at server.threads, found string".into(),
                    at_threads
                ),
                (
                    "expected integer at server.ports[1], found string".into(),
                    Some((3, 13))
                ),
                (
                    "expected table at server.threads, found string".into(),
                    at_threads
                ),
                (
                    "expected table at server.threads, found string".into(),
                    at_threads
                ),
            ]
        );
    }

    #[test]
    fn parsed_values_accept_strings_and_numbers() {
        let d = doc("a = '8'\nb = 8\nc = 2.5\nd = true\ne = 'x'\n");
        let r = Reader::new(&d);
        assert_eq!(r.parsed::<u64>("a").unwrap(), 8);
        assert_eq!(r.parsed::<u64>("b").unwrap(), 8);
        assert_eq!(r.parsed::<f32>("c").unwrap(), 2.5);
        let err = r.parsed::<u64>("d").unwrap_err();
        assert_eq!(err.message(), "expected string at d, found boolean");
        let err = r.parsed::<u64>("e").unwrap_err();
        assert_eq!(
            err.message(),
            "invalid value for e: invalid digit found in string"
        );
        assert!(r.opt_parsed::<IpAddr>("zzz").unwrap().is_none());
    }

    #[test]
    fn scoped_readers_resolve_relative_paths() {
        let d = doc("[storage]\ndir = '/var/lib/belay'\n[storage.wal]\nsegment_bytes = 1024\n");
        let r = Reader::new(&d);
        let storage = r.table("storage").unwrap().unwrap();
        assert_eq!(storage.path().to_string(), "storage");
        assert_eq!(storage.keys(), ["dir", "wal"]);
        assert_eq!(storage.string("dir").unwrap(), "/var/lib/belay");
        let wal = storage.table("wal").unwrap().unwrap();
        assert_eq!(wal.req::<u64>("segment_bytes").unwrap(), 1024);
        assert!(r.table("nothing").unwrap().is_none());
        assert!(r.tables("nothing").unwrap().is_empty());
        assert!(r.unused_keys().is_empty());
        let err = storage.req::<String>("missing").unwrap_err();
        assert_eq!(err.message(), "missing required key `storage.missing`");
    }

    #[test]
    fn malformed_paths_are_errors_not_panics() {
        let d = doc("a = 1");
        let r = Reader::new(&d);
        assert!(matches!(r.opt_int("a..b"), Err(Error::Path { .. })));
        assert!(matches!(r.invalid("[", "x"), Error::Path { .. }));
    }
}
