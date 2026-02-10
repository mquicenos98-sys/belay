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
