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

/// Replaces every placeholder in `payload`. `result_of(step)` returns the
/// step's result, or why there is none; the first missing value aborts
/// with a message naming its placeholder.
pub(crate) fn render(
    payload: &Value,
    result_of: &mut dyn FnMut(&str) -> Result<Value, String>,
) -> Result<Value, String> {
    match payload {
        Value::String(s) => render_string(s, result_of),
        Value::Array(items) => items
            .iter()
            .map(|item| render(item, result_of))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(map) => {
            let mut out = map.clone();
            for (_, item) in out.iter_mut() {
                *item = render(item, result_of)?;
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

fn render_string(
    s: &str,
    result_of: &mut dyn FnMut(&str) -> Result<Value, String>,
) -> Result<Value, String> {
    let pieces = scan(s).map_err(|e| e.to_string())?;
    if let [Piece::Hole(hole)] = pieces.as_slice() {
        return resolve(hole, result_of);
    }
    if !pieces.iter().any(|p| matches!(p, Piece::Hole(_))) {
        return Ok(Value::String(s.to_owned()));
    }
    let mut out = String::with_capacity(s.len());
    for piece in &pieces {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Hole(hole) => {
                out.push_str(&belay_json::to_string(&resolve(hole, result_of)?));
            }
        }
    }
    Ok(Value::String(out))
}

fn resolve(
    hole: &Placeholder,
    result_of: &mut dyn FnMut(&str) -> Result<Value, String>,
) -> Result<Value, String> {
    let result =
        result_of(&hole.step).map_err(|why| format!("placeholder {}: {why}", hole.source))?;
    hole.pointer.get(&result).cloned().ok_or_else(|| {
        format!(
            "placeholder {}: the result of step {:?} has no value at {}",
            hole.source, hole.step, hole.pointer
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_json::json;

    fn results(step: &str) -> Result<Value, String> {
        match step {
            "fetch" => {
                Ok(json!({"url": "http://x/a", "items": [{"id": 7}, {"id": 8}], "a/b": true}))
            }
            "count" => Ok(json!(42)),
            "nothing" => Ok(Value::Null),
            other => Err(format!("step {other:?} was skipped, so it has no result")),
        }
    }

    fn render_ok(payload: Value) -> Value {
        render(&payload, &mut results).unwrap()
    }

    #[test]
    fn whole_string_placeholders_keep_the_value_type() {
        assert_eq!(render_ok(json!("{{steps.count.result}}")), json!(42));
        assert_eq!(
            render_ok(json!({"items": "{{ steps.fetch.result/items }}"})),
            json!({"items": [{"id": 7}, {"id": 8}]})
        );
        assert_eq!(
            render_ok(json!(["{{steps.nothing.result}}"])),
            json!([null])
        );
    }

    #[test]
    fn dotted_paths_and_pointers_agree() {
        let dotted = render_ok(json!("{{steps.fetch.result.items.1.id}}"));
        let pointer = render_ok(json!("{{steps.fetch.result/items/1/id}}"));
        assert_eq!(dotted, json!(8));
        assert_eq!(pointer, dotted);
        // A dotted segment containing '/' is escaped, not split.
        assert_eq!(render_ok(json!("{{steps.fetch.result.a/b}}")), json!(true));
        assert_eq!(render_ok(json!("{{steps.fetch.result/a~1b}}")), json!(true));
    }

    #[test]
    fn other_braces_and_keys_are_left_alone() {
        let payload = json!({
            "{{steps.count.result}}": "key stays",
            "mustache": "Hello {{ name }}",
            "open": "{{steps.count.result",
            "n": 5
        });
        assert_eq!(render_ok(payload.clone()), payload);
        assert!(!has_placeholders(&json!({"a": "{{ name }}"})));
        assert!(has_placeholders(&json!({"a": ["x", "{{steps.a.result}}"]})));
    }

    #[test]
    fn missing_values_name_the_placeholder() {
        let err = render(&json!({"x": "{{steps.fetch.result/nope}}"}), &mut results).unwrap_err();
        assert_eq!(
            err,
            r#"placeholder {{steps.fetch.result/nope}}: the result of step "fetch" has no value at /nope"#
        );
        let err = render(&json!("id {{steps.load.result.id}}"), &mut results).unwrap_err();
        assert_eq!(
            err,
            r#"placeholder {{steps.load.result.id}}: step "load" was skipped, so it has no result"#
        );
    }

    #[test]
    fn malformed_placeholders_are_found_with_their_location() {
        let payload = json!({
            "ok": "{{steps.a.result}}",
            "list": ["fine", "{{steps.a.results}}"],
            "deep": {"x": "{{steps..result}}", "y": "{{steps.a.result.x..y}}", "z": "{{steps.a.result~x}}"},
            "bad_pointer": "{{steps.a.result/~2}}"
        });
        let found: Vec<String> = check(&payload)
            .into_iter()
            .map(|(at, e)| format!("{at}: {e}"))
            .collect();
        assert_eq!(
            found,
            [
                r#"/list/1: invalid placeholder {{steps.a.results}}: unexpected "s" after "result" (use /json/pointer or .dotted.keys)"#,
                r#"/deep/x: invalid placeholder {{steps..result}}: expected a step name after "steps.""#,
                "/deep/y: invalid placeholder {{steps.a.result.x..y}}: empty key in dotted path",
                r#"/deep/z: invalid placeholder {{steps.a.result~x}}: unexpected "~x" after "result" (use /json/pointer or .dotted.keys)"#,
                "/bad_pointer: invalid placeholder {{steps.a.result/~2}}: invalid JSON pointer at offset 1: '~' must be followed by '0' or '1'",
            ]
        );
        let err = check(&json!("{{steps.fetch}}")).remove(0).1;
        assert_eq!(
            err.to_string(),
            r#"invalid placeholder {{steps.fetch}}: expected "steps.fetch.result", optionally followed by a path"#
        );
    }
}
