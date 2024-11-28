//! The [`Value`] tree, its accessors and indexing.

mod from;

use std::fmt;
use std::mem;
use std::ops;
use std::str::FromStr;

use crate::parse::ParseError;
use crate::write::{self, WriteOptions};
use crate::{Map, Number};

/// Any JSON value.
///
/// `Display` writes compact JSON; the alternate flag (`{:#}`) writes the
/// two-space pretty form. Equality is structural, with the number semantics
/// described on [`Number`] and order-insensitive objects (see [`Map`]).
///
/// ```
/// use belay_json::{json, Value};
///
/// let job = json!({"queue": "emails", "attempts": [1, 2], "tag": null});
/// assert_eq!(job["queue"], "emails");
/// assert_eq!(job["attempts"][1], 2);
/// assert!(job["tag"].is_null());
/// assert!(job["no such key"].is_null());
/// assert_eq!(job.to_string(), r#"{"queue":"emails","attempts":[1,2],"tag":null}"#);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Value {
    /// `null`.
    #[default]
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// Any number; see [`Number`] for how integers and floats are kept.
    Number(Number),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Value>),
    /// An object, in document order.
    Object(Map),
}

/// The type of a [`Value`], for messages like "expected string, found array".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `null`
    Null,
    /// `true` / `false`
    Bool,
    /// numbers
    Number,
    /// strings
    String,
    /// arrays
    Array,
    /// objects
    Object,
}
