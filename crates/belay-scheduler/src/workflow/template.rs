//! Step payload templating: `{{steps.<name>.result...}}` placeholders.
//!
//! # The rule
//!
//! A placeholder is `{{steps.<name>.result<path>}}`. `<path>` selects part
//! of the result and is either empty (the whole result), an RFC 6901 JSON
//! Pointer (`/items/0/url`), or dotted keys (`.items.0.url`, shorthand for
//! the pointer with the same segments, each escaped). Spaces just inside
//! the braces are ignored.
//!
//! - A JSON **string that is exactly one placeholder** is replaced by the
//!   referenced value, whatever its type: `"{{steps.count.result}}"`
//!   becomes the number `42`, `"{{steps.fetch.result.urls}}"` an array.
//! - Placeholders **inside a longer string** are replaced by text: string
//!   values as they are, anything else as compact JSON (`null`, `42`,
//!   `{"a":1}`). `"page {{steps.count.result}} of 9"` becomes
//!   `"page 42 of 9"`.
//! - Only string *values* are scanned, at any depth; object keys are not.
//! - `{{...}}` markers that do not start with `steps.` are left untouched,
//!   so payloads can carry templates meant for other tools.
//! - A placeholder whose value is missing fails the step with an error
//!   naming the placeholder: the referenced step did not succeed, or the
//!   path leads nowhere in its result.
//!
//! There is no escape syntax: a payload cannot contain the literal text
//! `{{steps.` followed by `}}` without it being treated as a placeholder.

use std::fmt;

use belay_json::{Pointer, Value};

const OPEN: &str = "{{";

const CLOSE: &str = "}}";

const PREFIX: &str = "steps.";

const ATTRIBUTE: &str = "result";

/// A parsed placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Placeholder {
    /// The step whose result it reads.
    pub step: String,
    /// Where in the result.
    pub pointer: Pointer,
    /// The placeholder as written, braces included, for messages.
    pub source: String,
}

/// A piece of a scanned string.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece<'a> {
    Text(&'a str),
    Hole(Placeholder),
}

/// A malformed placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyntaxError {
    source: String,
    reason: String,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid placeholder {}: {}", self.source, self.reason)
    }
}
