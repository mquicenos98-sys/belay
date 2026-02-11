//! Reading typed values out of a configuration document: durations, byte
//! sizes, and the domain objects (`QueueConfig`, `ScheduleSpec`,
//! `CalendarSpec`) that already have a JSON form in belay-proto.
//!
//! Domain objects are read by converting their TOML table to JSON and
//! decoding it with the same `FromJson` and `Validate` code the HTTP API
//! uses, so a queue configured in a file and one configured over the API
//! accept exactly the same fields. Errors from either step carry a JSON
//! path relative to the table, which is joined onto the table's key path
//! so the diagnostic points at the offending line.

use std::time::Duration;

use belay_common::parse_duration;
use belay_common::text::parse_bytes;
use belay_conf::{ConvertError, KeyPath, Reader, Result, Segment, Value};
use belay_json::{FromJson, FromJsonError, Map, Number, PathSegment};
use belay_proto::validate::Validate;

/// The duration at `path`: a string in `parse_duration` syntax (`"30s"`,
/// `"1h30m"`, or a bare number of seconds), or an integer of seconds.
pub(crate) fn duration(r: &Reader<'_>, path: &str, default: Duration) -> Result<Duration> {
    Ok(r.opt_with(path, duration_value)?.unwrap_or(default))
}

fn duration_value(v: &Value) -> std::result::Result<Duration, ConvertError> {
    match v {
        Value::String(s) => parse_duration(s).map_err(|e| ConvertError::invalid(e.to_string())),
        Value::Integer(n) => u64::try_from(*n)
            .map(Duration::from_secs)
            .map_err(|_| ConvertError::invalid(format!("must not be negative, got {n}"))),
        other => Err(ConvertError::mismatch("duration", other)),
    }
}

/// The byte size at `path`: an integer, or a string such as `"64MiB"`.
pub(crate) fn bytes(r: &Reader<'_>, path: &str, default: u64) -> Result<u64> {
    let value = r.opt_with(path, |v: &Value| match v {
        Value::String(s) => parse_bytes(s).map_err(|e| ConvertError::invalid(e.to_string())),
        Value::Integer(n) => u64::try_from(*n)
            .map_err(|_| ConvertError::invalid(format!("must not be negative, got {n}"))),
        other => Err(ConvertError::mismatch("byte size", other)),
    })?;
    Ok(value.unwrap_or(default))
}

/// Reads the whole table (or array) at `path` as a domain object, decoded
/// by its JSON form and validated. `None` if absent.
pub(crate) fn domain<T>(r: &Reader<'_>, path: &str) -> Result<Option<T>>
where
    T: FromJson + Validate,
{
    let Some(value) = r.value(path)? else {
        return Ok(None);
    };
    let base = KeyPath::parse(path)?;
    let json = to_json(value).map_err(|(at, message)| located(r, &base.join(&at), message))?;
    let decoded = T::from_json(&json).map_err(|e| from_json_error(r, &base, &e))?;
    if let Err(violations) = decoded.validate() {
        let first = violations.first();
        let at = pointer_path(&first.pointer());
        let mut message = first.message().to_owned();
        if violations.len() > 1 {
            let more = violations.len() - 1;
            message.push_str(&format!(
                " (and {more} more problem{})",
                if more == 1 { "" } else { "s" }
            ));
        }
        return Err(located(r, &base.join(&at), message));
    }
    Ok(Some(decoded))
}

/// An error located at `path` (relative to `r`).
pub(crate) fn located(
    r: &Reader<'_>,
    path: &KeyPath,
    message: impl Into<String>,
) -> belay_conf::Error {
    r.invalid(&path.to_string(), message)
}

fn from_json_error(r: &Reader<'_>, base: &KeyPath, e: &FromJsonError) -> belay_conf::Error {
    let mut at = base.clone();
    for segment in e.path() {
        at = match segment {
            PathSegment::Key(k) => at.key(k),
            PathSegment::Index(i) => at.index(*i),
        };
    }
    // An unknown field is reported against its object; point at the key
    // itself, which is the line the user has to fix.
    let unknown = e
        .message()
        .strip_prefix("unknown field \"")
        .and_then(|rest| rest.split_once('"'))
        .map(|(field, _)| field);
    if let Some(field) = unknown {
        at = at.key(field);
    }
    located(r, &at, e.message())
}

/// Converts an RFC 6901 pointer (`/steps/1/depends_on/0`) into a key path.
/// All-digit tokens are taken as array indices, which is what they are in
/// every structure read here.
fn pointer_path(pointer: &str) -> KeyPath {
    let mut path = KeyPath::root();
    for token in pointer.split('/').skip(1) {
        let token = token.replace("~1", "/").replace("~0", "~");
        path = match token.parse::<usize>() {
            Ok(i) if token.bytes().all(|b| b.is_ascii_digit()) => path.index(i),
            _ => path.key(&token),
        };
    }
    path
}

/// Converts TOML to JSON. Date-times become strings, which is how the
/// JSON forms spell timestamps. Fails, with the path of the value, only for
/// floats JSON cannot represent (`inf`, `nan`).
pub(crate) fn to_json(value: &Value) -> std::result::Result<belay_json::Value, (KeyPath, String)> {
    Ok(match value {
        Value::String(s) | Value::Datetime(s) => belay_json::Value::String(s.clone()),
        Value::Integer(i) => belay_json::Value::Number(Number::from(*i)),
        Value::Float(f) => match Number::from_f64(*f) {
            Some(n) => belay_json::Value::Number(n),
            None => return Err((KeyPath::root(), format!("{f} cannot be represented here"))),
        },
        Value::Boolean(b) => belay_json::Value::Bool(*b),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                out.push(to_json(item).map_err(|(at, m)| (prefixed(Segment::Index(i), &at), m))?);
            }
            belay_json::Value::Array(out)
        }
        Value::Table(table) => {
            let mut map = Map::with_capacity(table.len());
            for (key, item) in table {
                let converted = to_json(item)
                    .map_err(|(at, m)| (prefixed(Segment::Key(key.to_owned()), &at), m))?;
                map.insert(key, converted);
            }
            belay_json::Value::Object(map)
        }
    })
}

fn prefixed(first: Segment, rest: &KeyPath) -> KeyPath {
    let head = match first {
        Segment::Key(k) => KeyPath::root().key(&k),
        Segment::Index(i) => KeyPath::root().index(i),
    };
    head.join(rest)
}

/// Converts JSON back to TOML, for printing an effective configuration.
/// `null` members are dropped (TOML has no null), which matches how the
/// JSON forms treat absent optional fields.
pub(crate) fn from_json(value: &belay_json::Value) -> Option<Value> {
    Some(match value {
        belay_json::Value::Null => return None,
        belay_json::Value::Bool(b) => Value::Boolean(*b),
        belay_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Integer(i),
            None => Value::Float(n.as_f64()),
        },
        belay_json::Value::String(s) => Value::String(s.clone()),
        belay_json::Value::Array(items) => {
            Value::Array(items.iter().filter_map(from_json).collect())
        }
        belay_json::Value::Object(map) => {
            let mut table = belay_conf::Table::new();
            for (key, item) in map.iter() {
                if let Some(v) = from_json(item) {
                    table.insert(key, v);
                }
            }
            Value::Table(table)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_conf::Document;
    use belay_proto::QueueConfig;

    #[test]
    fn durations_accept_text_and_whole_seconds() {
        let doc = Document::parse("a = '1m30s'\nb = 45\nc = -1\nd = true\ne = 'soon'").unwrap();
        let r = Reader::new(&doc);
        let d = Duration::from_secs(7);
        assert_eq!(duration(&r, "a", d).unwrap(), Duration::from_secs(90));
        assert_eq!(duration(&r, "b", d).unwrap(), Duration::from_secs(45));
        assert_eq!(duration(&r, "missing", d).unwrap(), d);
        assert_eq!(
            duration(&r, "c", d).unwrap_err().message(),
            "invalid value for c: must not be negative, got -1"
        );
        assert_eq!(
            duration(&r, "d", d).unwrap_err().message(),
            "expected duration at d, found boolean"
        );
        assert!(duration(&r, "e", d)
            .unwrap_err()
            .message()
            .starts_with("invalid value for e"));
    }

    #[test]
    fn byte_sizes_accept_units() {
        let doc = Document::parse("a = '64MiB'\nb = 4096\nc = 'lots'").unwrap();
        let r = Reader::new(&doc);
        assert_eq!(bytes(&r, "a", 0).unwrap(), 64 << 20);
        assert_eq!(bytes(&r, "b", 0).unwrap(), 4096);
        assert_eq!(bytes(&r, "z", 9).unwrap(), 9);
        assert_eq!(bytes(&r, "c", 0).unwrap_err().line_col(), Some((3, 5)));
    }

    #[test]
    fn domain_errors_point_at_the_nested_value() {
        let src = "[q]\nmax_concurrency = 4\n[q.retention]\ndead = 'forever'\n";
        let doc = Document::parse(src).unwrap();
        let err = domain::<QueueConfig>(&Reader::new(&doc), "q").unwrap_err();
        assert!(
            err.message()
                .starts_with("invalid value for q.retention.dead:"),
            "{}",
            err.message()
        );
        assert_eq!(err.line_col(), Some((4, 8)));

        let doc = Document::parse("[q]\nmax_concurrency = 0\n").unwrap();
        let err = domain::<QueueConfig>(&Reader::new(&doc), "q").unwrap_err();
        assert_eq!(
            err.message(),
            "invalid value for q.max_concurrency: must be at least 1; omit it for no limit"
        );
        assert_eq!(err.line_col(), Some((2, 19)));
    }

    #[test]
    fn pointers_become_key_paths() {
        assert_eq!(
            pointer_path("/steps/1/depends_on/0").to_string(),
            "steps[1].depends_on[0]"
        );
        assert_eq!(pointer_path("/meta/a~1b").to_string(), "meta.\"a/b\"");
        assert!(pointer_path("").is_root());
    }

    #[test]
    fn json_and_toml_convert_both_ways() {
        let doc =
            Document::parse("[t]\na = [1, 2.5, 'x']\nb = { c = true }\nd = 2025-01-02T03:04:05Z\n")
                .unwrap();
        let t = doc.get("t").unwrap();
        let json = to_json(t).unwrap();
        assert_eq!(
            json.to_string(),
            r#"{"a":[1,2.5,"x"],"b":{"c":true},"d":"2025-01-02T03:04:05Z"}"#
        );
        let back = from_json(&json).unwrap();
        assert_eq!(
            back.as_table().unwrap().get("b"),
            t.as_table().unwrap().get("b")
        );
        let err = to_json(&belay_conf::parse_value("[1, inf]").unwrap()).unwrap_err();
        assert_eq!(err.0.to_string(), "[1]");
    }
}
