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

/// Splits `s` into text and placeholders.
fn scan(s: &str) -> Result<Vec<Piece<'_>>, SyntaxError> {
    let mut pieces = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find(OPEN) {
        let after = &rest[open + OPEN.len()..];
        let Some(close) = after.find(CLOSE) else {
            break;
        };
        let inner = after[..close].trim();
        let end = open + OPEN.len() + close + CLOSE.len();
        if inner.starts_with(PREFIX) {
            if open > 0 {
                pieces.push(Piece::Text(&rest[..open]));
            }
            pieces.push(Piece::Hole(parse(inner, &rest[open..end])?));
        } else {
            pieces.push(Piece::Text(&rest[..end]));
        }
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        pieces.push(Piece::Text(rest));
    }
    Ok(pieces)
}

/// Parses the inside of `{{ ... }}`, which starts with `steps.`.
fn parse(inner: &str, source: &str) -> Result<Placeholder, SyntaxError> {
    let fail = |reason: String| SyntaxError {
        source: source.to_owned(),
        reason,
    };
    let body = &inner[PREFIX.len()..];
    let name_len = body
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'))
        .unwrap_or(body.len());
    let (step, rest) = body.split_at(name_len);
    if step.is_empty() {
        return Err(fail("expected a step name after \"steps.\"".to_owned()));
    }
    let Some(path) = rest
        .strip_prefix('.')
        .and_then(|r| r.strip_prefix(ATTRIBUTE))
    else {
        return Err(fail(format!(
            "expected \"steps.{step}.result\", optionally followed by a path"
        )));
    };
    let pointer = if path.is_empty() {
        Pointer::root()
    } else if path.starts_with('/') {
        Pointer::parse(path).map_err(|e| fail(e.to_string()))?
    } else if let Some(dotted) = path.strip_prefix('.') {
        let mut pointer = Pointer::root();
        for segment in dotted.split('.') {
            if segment.is_empty() {
                return Err(fail("empty key in dotted path".to_owned()));
            }
            pointer.push(segment);
        }
        pointer
    } else {
        return Err(fail(format!(
            "unexpected {path:?} after \"result\" (use /json/pointer or .dotted.keys)"
        )));
    };
    Ok(Placeholder {
        step: step.to_owned(),
        pointer,
        source: source.to_owned(),
    })
}

/// Every malformed placeholder in `payload`, with the JSON Pointer of the
/// string that holds it.
pub(crate) fn check(payload: &Value) -> Vec<(Pointer, SyntaxError)> {
    let mut problems = Vec::new();
    walk_check(payload, &Pointer::root(), &mut problems);
    problems
}

fn walk_check(value: &Value, at: &Pointer, problems: &mut Vec<(Pointer, SyntaxError)>) {
    match value {
        Value::String(s) => {
            if let Err(e) = scan(s) {
                problems.push((at.clone(), e));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk_check(item, &at.child(i), problems);
            }
        }
        Value::Object(map) => {
            for (key, item) in map.iter() {
                walk_check(item, &at.child(key), problems);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Whether `payload` contains any placeholder.
pub(crate) fn has_placeholders(payload: &Value) -> bool {
    !belay_proto::workflow::referenced_steps(payload).is_empty()
}
