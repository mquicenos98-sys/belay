//! Line formats (logfmt and JSON) and the stream-writing logger.

use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::str::FromStr;
use std::sync::Mutex;

use super::{Filter, LogConfigError, Logger, Metadata, Record, Value};
use crate::sync::lock;
use crate::time::{system_clock, SharedClock, Timestamp};

/// Keys written for every record. User fields with these names get a
/// leading underscore so they cannot be mistaken for them.
const RESERVED_KEYS: [&str; 4] = ["ts", "level", "target", "msg"];

/// How records are rendered as lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LogFormat {
    /// `key=value` pairs, values quoted only when needed. Easy to read and
    /// to grep; understood by most log shippers.
    #[default]
    Logfmt,
    /// One JSON object per line.
    Json,
}

impl LogFormat {
    /// Appends one rendered record (without a trailing newline) to `out`.
    ///
    /// ```
    /// use belay_common::log::{Field, Level, LogFormat, Record, Value};
    /// use belay_common::time::Timestamp;
    ///
    /// let fields = [Field::new("job", Value::Str("01JAB")), Field::new("attempt", Value::U64(2))];
    /// let record = Record::new(Level::Info, "belay_queue", format_args!("lease expired"), &fields);
    /// let ts = Timestamp::from_millis(1_741_064_767_089);
    ///
    /// let mut line = String::new();
    /// LogFormat::Logfmt.write(ts, &record, &mut line);
    /// assert_eq!(
    ///     line,
    ///     r#"ts=2025-03-04T05:06:07.089Z level=info target=belay_queue msg="lease expired" job=01JAB attempt=2"#
    /// );
    ///
    /// line.clear();
    /// LogFormat::Json.write(ts, &record, &mut line);
    /// assert_eq!(
    ///     line,
    ///     r#"{"ts":"2025-03-04T05:06:07.089Z","level":"info","target":"belay_queue","msg":"lease expired","job":"01JAB","attempt":2}"#
    /// );
    /// ```
    pub fn write(self, ts: Timestamp, record: &Record<'_>, out: &mut String) {
        match self {
            LogFormat::Logfmt => write_logfmt(ts, record, out),
            LogFormat::Json => write_json(ts, record, out),
        }
    }

    /// `"logfmt"` or `"json"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LogFormat::Logfmt => "logfmt",
            LogFormat::Json => "json",
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Case-insensitive; `text` is accepted for `logfmt`.
impl FromStr for LogFormat {
    type Err = LogConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "logfmt" | "text" => Ok(LogFormat::Logfmt),
            "json" => Ok(LogFormat::Json),
            _ => Err(LogConfigError::UnknownFormat(s.to_owned())),
        }
    }
}

fn field_key(key: &str) -> (&'static str, &str) {
    if RESERVED_KEYS.contains(&key) {
        ("_", key)
    } else {
        ("", key)
    }
}

fn write_logfmt(ts: Timestamp, record: &Record<'_>, out: &mut String) {
    let mut scratch = String::new();
    let _ = write!(out, "ts={ts} level={} target=", record.level().as_str());
    logfmt_value(record.target(), out);
    out.push_str(" msg=");
    let _ = write!(scratch, "{}", record.message());
    logfmt_value(&scratch, out);

    for field in record.fields() {
        out.push(' ');
        let (prefix, key) = field_key(field.key());
        out.push_str(prefix);
        logfmt_key(key, out);
        out.push('=');
        match field.value() {
            Value::Bool(b) => {
                let _ = write!(out, "{b}");
            }
            Value::I64(n) => {
                let _ = write!(out, "{n}");
            }
            Value::U64(n) => {
                let _ = write!(out, "{n}");
            }
            Value::F64(x) => {
                let _ = write!(out, "{x}");
            }
            Value::Str(s) => logfmt_value(s, out),
            other => {
                scratch.clear();
                let _ = write!(scratch, "{other}");
                logfmt_value(&scratch, out);
            }
        }
    }
}

/// Keys cannot be quoted in logfmt, so characters that would break parsing
/// are replaced.
fn logfmt_key(key: &str, out: &mut String) {
    if key.is_empty() {
        out.push('_');
        return;
    }
    out.extend(key.chars().map(|c| {
        if c.is_alphanumeric() || "_.-:/".contains(c) {
            c
        } else {
            '_'
        }
    }));
}

fn logfmt_value(value: &str, out: &mut String) {
    let needs_quotes = value.is_empty()
        || value
            .chars()
            .any(|c| c == ' ' || c == '=' || c == '"' || c == '\\' || c.is_control());
    if !needs_quotes {
        out.push_str(value);
        return;
    }
    out.push('"');
    escape_into(value, out);
    out.push('"');
}

fn write_json(ts: Timestamp, record: &Record<'_>, out: &mut String) {
    let _ = write!(
        out,
        "{{\"ts\":\"{ts}\",\"level\":\"{}\",\"target\":",
        record.level().as_str()
    );
    json_string(record.target(), out);
    out.push_str(",\"msg\":");
    let mut scratch = String::new();
    let _ = write!(scratch, "{}", record.message());
    json_string(&scratch, out);

    for field in record.fields() {
        out.push(',');
        let (prefix, key) = field_key(field.key());
        out.push('"');
        out.push_str(prefix);
        escape_into(key, out);
        out.push_str("\":");
        match field.value() {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => {
                let _ = write!(out, "{b}");
            }
            Value::I64(n) => {
                let _ = write!(out, "{n}");
            }
            Value::U64(n) => {
                let _ = write!(out, "{n}");
            }
            // JSON has no NaN or infinity.
            Value::F64(x) if !x.is_finite() => out.push_str("null"),
            Value::F64(x) => {
                let _ = write!(out, "{x}");
            }
            Value::Str(s) => json_string(s, out),
            other => {
                scratch.clear();
                let _ = write!(scratch, "{other}");
                json_string(&scratch, out);
            }
        }
    }
    out.push('}');
}
