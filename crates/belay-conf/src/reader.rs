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
