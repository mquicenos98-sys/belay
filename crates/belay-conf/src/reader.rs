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
