//! Semantic validation: the rules that go beyond a value's JSON shape.
//!
//! Decoding (`FromJson`) checks structure and syntax: required fields, types,
//! identifier formats, duration strings. [`Validate`] checks policy: ranges,
//! limits from [`limits`](crate::limits), cross-field rules, workflow graphs.
//! The two are kept apart on purpose. Stored records are decoded on every
//! restart, and a limit tightened in a later release must not make old jobs
//! unreadable; it should only stop new ones from being accepted. Servers call
//! [`Validate::validate`] on every request body (see
//! [`api::decode_request`](crate::api::decode_request)); embedded callers
//! should do the same before handing a spec to the engine.
//!
//! A [`Validator`] collects every problem instead of stopping at the first,
//! so a client fixing a request sees all of them at once. Each problem has a
//! path in the same `$.jobs[2].priority` notation that belay-json uses for
//! decoding errors.

use std::fmt;
use std::time::Duration;

use belay_common::format_duration;
use belay_json::{FromJsonError, PathSegment};

use crate::ids::NameRule;

/// Problems beyond this many are counted but not kept, so a batch of a
/// thousand broken jobs produces a bounded error response.
pub const MAX_REPORTED_VIOLATIONS: usize = 100;

/// A single broken rule, with the path to the offending value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    // FromJsonError already carries a path and renders it in the notation
    // used across belay; reusing it keeps the two error kinds identical.
    inner: FromJsonError,
}

impl Violation {
    fn new(path: &[PathSegment], message: String) -> Violation {
        let mut inner = FromJsonError::new(message);
        for segment in path.iter().rev() {
            inner = match segment {
                PathSegment::Key(k) => inner.at_key(k.clone()),
                PathSegment::Index(i) => inner.at_index(*i),
            };
        }
        Violation { inner }
    }

    /// The path in `$.steps[1].depends_on[0]` notation.
    #[must_use]
    pub fn path(&self) -> String {
        self.inner.path_string()
    }

    /// The path as an RFC 6901 JSON Pointer (`/steps/1/depends_on/0`).
    #[must_use]
    pub fn pointer(&self) -> String {
        self.inner.pointer()
    }

    /// The message without the path.
    #[must_use]
    pub fn message(&self) -> &str {
        self.inner.message()
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl From<FromJsonError> for Violation {
    fn from(inner: FromJsonError) -> Violation {
        Violation { inner }
    }
}

/// One or more violations. Never empty.
///
/// `Display` shows the first problem and how many others there are:
/// `$.priority: must be between -1000 and 1000, got 5000 (and 2 more
/// problems)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violations {
    list: Vec<Violation>,
    /// Problems found beyond [`MAX_REPORTED_VIOLATIONS`].
    omitted: usize,
}

impl Violations {
    /// A single violation at `path`, for callers outside a [`Validator`]
    /// such as query-string parsers.
    #[must_use]
    pub fn single(path: &[PathSegment], message: impl fmt::Display) -> Violations {
        Violations {
            list: vec![Violation::new(path, message.to_string())],
            omitted: 0,
        }
    }

    /// The first problem found.
    #[must_use]
    pub fn first(&self) -> &Violation {
        // Invariant: constructed only with at least one entry.
        &self.list[0]
    }

    /// The kept problems, in the order they were found.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Violation> {
        self.list.iter()
    }

    /// Total number of problems, including ones not kept.
    #[must_use]
    pub fn len(&self) -> usize {
        self.list.len() + self.omitted
    }

    /// Always false; present for API symmetry with `len`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl fmt::Display for Violations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.first())?;
        match self.len() - 1 {
            0 => Ok(()),
            1 => f.write_str(" (and 1 more problem)"),
            n => write!(f, " (and {n} more problems)"),
        }
    }
}

impl std::error::Error for Violations {}

impl From<FromJsonError> for Violations {
    fn from(e: FromJsonError) -> Violations {
        Violations {
            list: vec![e.into()],
            omitted: 0,
        }
    }
}

impl<'a> IntoIterator for &'a Violations {
    type Item = &'a Violation;
    type IntoIter = std::slice::Iter<'a, Violation>;

    fn into_iter(self) -> Self::IntoIter {
        self.list.iter()
    }
}
