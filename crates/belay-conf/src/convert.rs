//! Conversions from [`Value`] into Rust types, used by the typed getters of
//! [`Document`](crate::Document) and [`Reader`](crate::Reader).
//!
//! Conversion errors carry a path relative to the converted value (for an
//! element of a list, `[3]`); the caller joins it with the value's own path
//! and attaches the source location.

use std::collections::{BTreeMap, HashMap};

use crate::path::{KeyPath, Segment};
use crate::value::{Table, Value};

/// Why a [`Value`] could not be converted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertErrorKind {
    /// The value has the wrong type.
    Type {
        /// What the target type needs, e.g. `"integer"`.
        expected: &'static str,
        /// What the value is.
        found: &'static str,
    },
    /// The type fits but the value does not (range, format, validation).
    Invalid(String),
}

/// A failed conversion, located relative to the converted value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertError {
    path: KeyPath,
    kind: ConvertErrorKind,
}

impl ConvertError {
    /// The value is a `found` where an `expected` was needed.
    pub fn mismatch(expected: &'static str, found: &Value) -> ConvertError {
        ConvertError {
            path: KeyPath::root(),
            kind: ConvertErrorKind::Type {
                expected,
                found: found.type_name(),
            },
        }
    }

    /// The value is unacceptable; `message` says why, e.g. "must be at least 1, got 0".
    pub fn invalid(message: impl Into<String>) -> ConvertError {
        ConvertError {
            path: KeyPath::root(),
            kind: ConvertErrorKind::Invalid(message.into()),
        }
    }

    /// Marks the error as coming from the element or entry `segment` of the
    /// converted value. Container conversions call this on their children.
    #[must_use]
    pub fn within(mut self, segment: Segment) -> ConvertError {
        let mut path = KeyPath::root();
        path.push(segment);
        self.path = path.join(&self.path);
        self
    }

    /// Where inside the converted value the problem is.
    pub fn path(&self) -> &KeyPath {
        &self.path
    }

    /// What went wrong.
    pub fn kind(&self) -> &ConvertErrorKind {
        &self.kind
    }
}

impl From<String> for ConvertError {
    fn from(message: String) -> ConvertError {
        ConvertError::invalid(message)
    }
}
