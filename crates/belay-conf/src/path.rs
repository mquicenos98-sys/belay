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
