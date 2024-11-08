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
