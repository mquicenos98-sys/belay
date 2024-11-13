//! A small structured logger.
//!
//! Records carry a level, a target (the module path by default), a formatted
//! message and typed key/value fields. They are produced by the
//! [`error!`](crate::error), [`warn!`](crate::warn), [`info!`](crate::info),
//! [`debug!`](crate::debug) and [`trace!`](crate::trace) macros, whose syntax
//! is described in [`macros`], and consumed by a [`Logger`].
//!
//! The installed logger defaults to a [`StreamLogger`] writing logfmt lines
//! to stderr:
//!
//! ```text
//! ts=2025-03-04T05:06:07.089Z level=info target=belay_queue::lease msg="lease expired" job=01JAB… worker=w1
//! ```
//!
//! A disabled statement costs one relaxed atomic load: the macros compare
//! the level with the global maximum before evaluating any argument.
//!
//! Tests capture output with a [`CaptureLogger`], usually scoped to the
//! current thread with [`CaptureLogger::scope`] so parallel tests do not see
//! each other's records.

mod capture;

mod filter;

mod format;

pub mod macros;

use std::cell::RefCell;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

pub use capture::{CaptureLogger, CapturedRecord};
pub use filter::Filter;
pub use format::{LogFormat, StreamLogger};

use crate::duration::CompactDuration;
use crate::sync::{read, write};

/// Severity of a record, from most to least severe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Something failed and needs attention.
    Error = 1,
    /// Something unexpected that belay recovered from.
    Warn,
    /// Normal operational events.
    Info,
    /// Detail useful when diagnosing a problem.
    Debug,
    /// Very verbose detail, e.g. per-request internals.
    Trace,
}

impl Level {
    /// Every level, most severe first.
    pub const ALL: [Level; 5] = [
        Level::Error,
        Level::Warn,
        Level::Info,
        Level::Debug,
        Level::Trace,
    ];

    /// Lowercase name as used in output and configuration.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Case-insensitive; `warning` is accepted for `warn`.
impl FromStr for Level {
    type Err = LogConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse::<LevelFilter>()?.to_level() {
            Some(level) => Ok(level),
            None => Err(LogConfigError::UnknownLevel(s.to_owned())),
        }
    }
}

/// A maximum level, or [`Off`](LevelFilter::Off) to disable logging.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LevelFilter {
    /// Nothing is logged.
    Off = 0,
    /// Only errors.
    Error,
    /// Errors and warnings.
    Warn,
    /// Up to info (the default).
    #[default]
    Info,
    /// Up to debug.
    Debug,
    /// Everything.
    Trace,
}

impl LevelFilter {
    /// Whether records at `level` pass this filter.
    #[must_use]
    pub const fn allows(self, level: Level) -> bool {
        level as usize <= self as usize
    }

    /// The most verbose level allowed, or `None` for `Off`.
    #[must_use]
    pub const fn to_level(self) -> Option<Level> {
        match self {
            LevelFilter::Off => None,
            LevelFilter::Error => Some(Level::Error),
            LevelFilter::Warn => Some(Level::Warn),
            LevelFilter::Info => Some(Level::Info),
            LevelFilter::Debug => Some(Level::Debug),
            LevelFilter::Trace => Some(Level::Trace),
        }
    }

    /// Lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self.to_level() {
            Some(level) => level.as_str(),
            None => "off",
        }
    }

    const fn from_usize(n: usize) -> LevelFilter {
        match n {
            0 => LevelFilter::Off,
            1 => LevelFilter::Error,
            2 => LevelFilter::Warn,
            3 => LevelFilter::Info,
            4 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        }
    }
}

impl From<Level> for LevelFilter {
    fn from(level: Level) -> Self {
        LevelFilter::from_usize(level as usize)
    }
}

impl fmt::Display for LevelFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for LevelFilter {
    type Err = LogConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let filter = match s.to_ascii_lowercase().as_str() {
            "off" => LevelFilter::Off,
            "error" => LevelFilter::Error,
            "warn" | "warning" => LevelFilter::Warn,
            "info" => LevelFilter::Info,
            "debug" => LevelFilter::Debug,
            "trace" => LevelFilter::Trace,
            _ => return Err(LogConfigError::UnknownLevel(s.to_owned())),
        };
        Ok(filter)
    }
}

/// Error from parsing a [`Level`], [`LevelFilter`], [`Filter`] or
/// [`LogFormat`] from configuration text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LogConfigError {
    /// Not one of `off`, `error`, `warn`, `info`, `debug`, `trace`.
    UnknownLevel(String),
    /// Not `logfmt` or `json`.
    UnknownFormat(String),
    /// A filter directive such as `=debug` without a target.
    EmptyTarget(String),
}

impl fmt::Display for LogConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogConfigError::UnknownLevel(s) => write!(
                f,
                "unknown log level {s:?} (expected off, error, warn, info, debug or trace)"
            ),
            LogConfigError::UnknownFormat(s) => {
                write!(f, "unknown log format {s:?} (expected logfmt or json)")
            }
            LogConfigError::EmptyTarget(directive) => {
                write!(f, "log filter directive {directive:?} has an empty target")
            }
        }
    }
}

impl std::error::Error for LogConfigError {}

/// A field value. Built by the logging macros: `key = v` uses [`ToValue`],
/// `key = %v` uses `Display` and `key = ?v` uses `Debug`.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum Value<'a> {
    /// An absent value (`None`).
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed integer.
    I64(i64),
    /// An unsigned integer.
    U64(u64),
    /// A float.
    F64(f64),
    /// A string.
    Str(&'a str),
    /// A duration, rendered compactly (`1m30s`).
    Duration(Duration),
    /// Anything rendered with `Display`.
    Display(&'a dyn fmt::Display),
    /// Anything rendered with `Debug`.
    Debug(&'a dyn fmt::Debug),
}

/// Renders the bare value, without logfmt quoting or JSON escaping.
impl fmt::Display for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::I64(n) => write!(f, "{n}"),
            Value::U64(n) => write!(f, "{n}"),
            Value::F64(x) => write!(f, "{x}"),
            Value::Str(s) => f.write_str(s),
            Value::Duration(d) => write!(f, "{}", CompactDuration(*d)),
            Value::Display(d) => write!(f, "{d}"),
            Value::Debug(d) => write!(f, "{d:?}"),
        }
    }
}

impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => write!(f, "{s:?}"),
            other => write!(f, "{other}"),
        }
    }
}

/// Conversion used for plain `key = value` fields. Implemented for
/// primitives, strings, `Duration`, `Option` and belay's own value types;
/// anything else can be logged with `%` (Display) or `?` (Debug).
pub trait ToValue {
    /// Borrows `self` as a field value.
    fn to_value(&self) -> Value<'_>;
}

macro_rules! to_value_via {
    ($variant:ident as $target:ty: $($t:ty),*) => {$(
        impl ToValue for $t {
            fn to_value(&self) -> Value<'_> {
                Value::$variant(*self as $target)
            }
        }
    )*};
}

to_value_via!(I64 as i64: i8, i16, i32, i64, isize);
to_value_via!(U64 as u64: u8, u16, u32, u64, usize);
to_value_via!(F64 as f64: f32, f64);

impl ToValue for bool {
    fn to_value(&self) -> Value<'_> {
        Value::Bool(*self)
    }
}

impl ToValue for str {
    fn to_value(&self) -> Value<'_> {
        Value::Str(self)
    }
}

impl ToValue for String {
    fn to_value(&self) -> Value<'_> {
        Value::Str(self)
    }
}

impl ToValue for std::borrow::Cow<'_, str> {
    fn to_value(&self) -> Value<'_> {
        Value::Str(self)
    }
}

impl ToValue for char {
    fn to_value(&self) -> Value<'_> {
        Value::Display(self)
    }
}

impl ToValue for Duration {
    fn to_value(&self) -> Value<'_> {
        Value::Duration(*self)
    }
}

impl ToValue for crate::time::Timestamp {
    fn to_value(&self) -> Value<'_> {
        Value::Display(self)
    }
}

impl ToValue for crate::id::Ulid {
    fn to_value(&self) -> Value<'_> {
        Value::Display(self)
    }
}

impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Value<'_> {
        self.as_ref().map_or(Value::Null, ToValue::to_value)
    }
}

impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Value<'_> {
        (**self).to_value()
    }
}

impl ToValue for Value<'_> {
    fn to_value(&self) -> Value<'_> {
        *self
    }
}

/// A key/value pair attached to a record.
#[derive(Clone, Copy, Debug)]
pub struct Field<'a> {
    key: &'a str,
    value: Value<'a>,
}

impl<'a> Field<'a> {
    /// A field; keys are usually identifiers such as `job` or `attempt`.
    #[must_use]
    pub const fn new(key: &'a str, value: Value<'a>) -> Field<'a> {
        Field { key, value }
    }

    /// The key.
    #[must_use]
    pub fn key(&self) -> &'a str {
        self.key
    }

    /// The value.
    #[must_use]
    pub fn value(&self) -> Value<'a> {
        self.value
    }
}

/// The level and target of a record, available before it is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata<'a> {
    level: Level,
    target: &'a str,
}

impl<'a> Metadata<'a> {
    /// Metadata for a record at `level` from `target`.
    #[must_use]
    pub const fn new(level: Level, target: &'a str) -> Metadata<'a> {
        Metadata { level, target }
    }

    /// The level.
    #[must_use]
    pub fn level(&self) -> Level {
        self.level
    }

    /// The target, usually a module path such as `belay_queue::lease`.
    #[must_use]
    pub fn target(&self) -> &'a str {
        self.target
    }
}

/// A log record as handed to a [`Logger`].
#[derive(Clone, Debug)]
pub struct Record<'a> {
    metadata: Metadata<'a>,
    message: fmt::Arguments<'a>,
    fields: &'a [Field<'a>],
    module_path: &'static str,
    file: &'static str,
    line: u32,
}

impl<'a> Record<'a> {
    /// Builds a record by hand, for code that assembles fields at run time
    /// or tests a [`Logger`]; emit it with [`log_record`]. Location fields
    /// are left empty.
    #[must_use]
    pub fn new(
        level: Level,
        target: &'a str,
        message: fmt::Arguments<'a>,
        fields: &'a [Field<'a>],
    ) -> Record<'a> {
        Record {
            metadata: Metadata::new(level, target),
            message,
            fields,
            module_path: "",
            file: "",
            line: 0,
        }
    }

    /// Level and target.
    #[must_use]
    pub fn metadata(&self) -> Metadata<'a> {
        self.metadata
    }

    /// The level.
    #[must_use]
    pub fn level(&self) -> Level {
        self.metadata.level
    }

    /// The target.
    #[must_use]
    pub fn target(&self) -> &'a str {
        self.metadata.target
    }

    /// The message; format it with `Display` (`record.message().to_string()`).
    #[must_use]
    pub fn message(&self) -> &fmt::Arguments<'a> {
        &self.message
    }

    /// The structured fields in the order they were written.
    #[must_use]
    pub fn fields(&self) -> &'a [Field<'a>] {
        self.fields
    }

    /// Module path of the logging statement.
    #[must_use]
    pub fn module_path(&self) -> &'static str {
        self.module_path
    }

    /// Source file of the logging statement.
    #[must_use]
    pub fn file(&self) -> &'static str {
        self.file
    }

    /// Source line of the logging statement.
    #[must_use]
    pub fn line(&self) -> u32 {
        self.line
    }
}

/// A destination for log records.
pub trait Logger: Send + Sync {
    /// Whether a record with this metadata would be logged. Called before
    /// the record's fields are evaluated.
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        let _ = metadata;
        true
    }

    /// Writes one record.
    fn log(&self, record: &Record<'_>);

    /// Flushes buffered output.
    fn flush(&self) {}
}

static MAX_LEVEL: AtomicUsize = AtomicUsize::new(LevelFilter::Info as usize);
static LOGGER: RwLock<Option<Arc<dyn Logger>>> = RwLock::new(None);
/// Number of active thread-scoped loggers across all threads; lets the
/// fast path skip the thread-local lookup when nobody uses them.
static SCOPED_ACTIVE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static SCOPED: RefCell<Option<Arc<dyn Logger>>> = const { RefCell::new(None) };
}

/// Sets the most verbose level that reaches the global logger. Accepts a
/// [`Level`] or a [`LevelFilter`]. The default is `Info`.
pub fn set_max_level(filter: impl Into<LevelFilter>) {
    MAX_LEVEL.store(filter.into() as usize, Ordering::Relaxed);
}

/// The current global maximum level.
#[must_use]
pub fn max_level() -> LevelFilter {
    LevelFilter::from_usize(MAX_LEVEL.load(Ordering::Relaxed))
}

/// Replaces the global logger, flushing the previous one.
pub fn set_logger(logger: Box<dyn Logger>) {
    let previous = write(&LOGGER).replace(Arc::from(logger));
    if let Some(previous) = previous {
        previous.flush();
    }
}

/// Installs a [`StreamLogger`] on stderr with the given filter and format,
/// and sets the global maximum level to match the filter.
pub fn init_stderr(filter: Filter, format: LogFormat) {
    set_max_level(filter.max_level());
    set_logger(Box::new(
        StreamLogger::stderr()
            .with_filter(filter)
            .with_format(format),
    ));
}

fn global_logger() -> Arc<dyn Logger> {
    static DEFAULT: OnceLock<Arc<dyn Logger>> = OnceLock::new();
    match &*read(&LOGGER) {
        Some(logger) => Arc::clone(logger),
        None => Arc::clone(DEFAULT.get_or_init(|| Arc::new(StreamLogger::stderr()))),
    }
}
