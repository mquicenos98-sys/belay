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

fn json_string(s: &str, out: &mut String) {
    out.push('"');
    escape_into(s, out);
    out.push('"');
}

/// Escaping shared by both formats: valid inside a JSON string and inside a
/// quoted logfmt value.
fn escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// Writes each record as one line to a byte stream (stderr by default).
///
/// Lines are written with a single `write_all` under a mutex, so records
/// from concurrent threads never interleave. Write errors are ignored:
/// failing to log must not take the process down.
pub struct StreamLogger {
    out: Mutex<Box<dyn Write + Send>>,
    format: LogFormat,
    filter: Filter,
    clock: SharedClock,
}

impl StreamLogger {
    /// Logfmt to stderr, filtered at `info`, stamped with the system clock.
    #[must_use]
    pub fn stderr() -> StreamLogger {
        StreamLogger::new(io::stderr())
    }

    /// Logfmt to `writer`, filtered at `info`, stamped with the system clock.
    #[must_use]
    pub fn new(writer: impl Write + Send + 'static) -> StreamLogger {
        StreamLogger {
            out: Mutex::new(Box::new(writer)),
            format: LogFormat::Logfmt,
            filter: Filter::default(),
            clock: system_clock(),
        }
    }

    /// Sets the line format.
    #[must_use]
    pub fn with_format(mut self, format: LogFormat) -> StreamLogger {
        self.format = format;
        self
    }

    /// Sets the per-target filter.
    #[must_use]
    pub fn with_filter(mut self, filter: Filter) -> StreamLogger {
        self.filter = filter;
        self
    }

    /// Sets the clock used for the `ts` key; handy for exact-output tests.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> StreamLogger {
        self.clock = clock;
        self
    }
}

impl Logger for StreamLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.filter.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        let mut line = String::with_capacity(160);
        self.format.write(self.clock.now(), record, &mut line);
        line.push('\n');
        let _ = lock(&self.out).write_all(line.as_bytes());
    }

    fn flush(&self) {
        let _ = lock(&self.out).flush();
    }
}

impl fmt::Debug for StreamLogger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamLogger")
            .field("format", &self.format)
            .field("filter", &self.filter)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{with_logger, Field, Level};
    use crate::time::ManualClock;
    use std::sync::Arc;

    /// A `Write` that appends into shared memory.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            lock(&self.0).extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn text(&self) -> String {
            String::from_utf8(lock(&self.0).clone()).expect("log output is UTF-8")
        }
    }

    fn render(format: LogFormat, fields: &[Field<'_>], msg: fmt::Arguments<'_>) -> String {
        let record = Record::new(Level::Warn, "t", msg, fields);
        let mut out = String::new();
        format.write(Timestamp::EPOCH, &record, &mut out);
        out
    }

    #[test]
    fn logfmt_quotes_only_when_needed() {
        let fields = [
            Field::new("a", Value::Str("plain")),
            Field::new("b", Value::Str("two words")),
            Field::new("c", Value::Str("")),
            Field::new("d", Value::Str("k=v")),
            Field::new("e", Value::Str("say \"hi\"\n")),
            Field::new("f", Value::Null),
            Field::new("g", Value::F64(0.25)),
            Field::new("h", Value::I64(-3)),
        ];
        assert_eq!(
            render(LogFormat::Logfmt, &fields, format_args!("m")),
            "ts=1970-01-01T00:00:00.000Z level=warn target=t msg=m a=plain b=\"two words\" c=\"\" d=\"k=v\" e=\"say \\\"hi\\\"\\n\" f=null g=0.25 h=-3"
        );
    }

    #[test]
    fn logfmt_sanitizes_keys_and_prefixes_reserved_ones() {
        let fields = [
            Field::new("has space", Value::Bool(true)),
            Field::new("msg", Value::Str("user")),
            Field::new("", Value::U64(1)),
        ];
        assert_eq!(
            render(LogFormat::Logfmt, &fields, format_args!("m")),
            "ts=1970-01-01T00:00:00.000Z level=warn target=t msg=m has_space=true _msg=user _=1"
        );
    }

    #[test]
    fn json_escapes_and_types_values() {
        let dur = crate::duration::CompactDuration(std::time::Duration::from_millis(1500));
        let fields = [
            Field::new("s", Value::Str("tab\there \u{1}")),
            Field::new("n", Value::U64(7)),
            Field::new("nan", Value::F64(f64::NAN)),
            Field::new("none", Value::Null),
            Field::new("dur", Value::Display(&dur)),
            Field::new("level", Value::Bool(false)),
        ];
        assert_eq!(
            render(LogFormat::Json, &fields, format_args!("quote \" backslash \\")),
            "{\"ts\":\"1970-01-01T00:00:00.000Z\",\"level\":\"warn\",\"target\":\"t\",\"msg\":\"quote \\\" backslash \\\\\",\"s\":\"tab\\there \\u0001\",\"n\":7,\"nan\":null,\"none\":null,\"dur\":\"1s500ms\",\"_level\":false}"
        );
    }

    #[test]
    fn stream_logger_writes_whole_lines_with_injected_clock() {
        let buf = SharedBuf::default();
        let clock = ManualClock::new(Timestamp::from_millis(1_741_064_767_089));
        let logger = StreamLogger::new(buf.clone())
            .with_clock(clock.shared())
            .with_filter(Filter::parse("warn,noisy=off").expect("valid filter"));
        with_logger(Arc::new(logger), || {
            crate::warn!("disk almost full"; free_pct = 3u8);
            crate::info!("filtered out");
            crate::error!(target: "noisy::sub", "silenced");
        });
        assert_eq!(
            buf.text(),
            format!(
                "ts=2025-03-04T05:06:07.089Z level=warn target={} msg=\"disk almost full\" free_pct=3\n",
                module_path!()
            )
        );
    }

    #[test]
    fn json_lines_from_many_threads_do_not_interleave() {
        let buf = SharedBuf::default();
        let logger: Arc<dyn Logger> =
            Arc::new(StreamLogger::new(buf.clone()).with_format(LogFormat::Json));
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let logger = Arc::clone(&logger);
                std::thread::spawn(move || {
                    with_logger(logger, || {
                        for i in 0..50 {
                            crate::info!("line"; thread = t, i);
                        }
                    });
                })
            })
            .collect();
        for t in threads {
            t.join().expect("logging thread panicked");
        }
        let text = buf.text();
        assert_eq!(text.lines().count(), 200);
        assert!(text
            .lines()
            .all(|l| l.starts_with("{\"ts\":") && l.ends_with('}')));
    }

    #[test]
    fn log_format_parses_names() {
        assert_eq!("JSON".parse::<LogFormat>(), Ok(LogFormat::Json));
        assert_eq!("text".parse::<LogFormat>(), Ok(LogFormat::Logfmt));
        assert_eq!(
            "xml".parse::<LogFormat>().unwrap_err().to_string(),
            "unknown log format \"xml\" (expected logfmt or json)"
        );
        assert_eq!(LogFormat::Json.to_string(), "json");
    }
}
