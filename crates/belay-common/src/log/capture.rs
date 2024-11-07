//! An in-memory logger for asserting on log output in tests.

use std::fmt;
use std::sync::{Arc, Mutex};

use super::{with_logger, Level, LevelFilter, Logger, Metadata, Record};
use crate::sync::lock;

/// A record as captured by [`CaptureLogger`], with the message and field
/// values already rendered to strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedRecord {
    /// The level.
    pub level: Level,
    /// The target.
    pub target: String,
    /// The formatted message.
    pub message: String,
    /// Fields in order, values rendered with their plain `Display` form.
    pub fields: Vec<(String, String)>,
}

impl CapturedRecord {
    /// The value of the first field named `key`.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Collects records in memory. Clones share the same buffer.
///
/// Prefer [`scope`](Self::scope), which captures only what the current
/// thread logs and so keeps parallel tests apart. To capture records from
/// other threads (a thread pool, a server), install a clone globally with
/// [`set_logger`](super::set_logger) and raise [`set_max_level`](super::set_max_level)
/// as needed; every test in the process then shares it.
///
/// ```
/// use belay_common::log::CaptureLogger;
///
/// let capture = CaptureLogger::new();
/// capture.scope(|| belay_common::info!("lease expired"; job = "j1"));
/// assert!(capture.contains("lease expired"));
/// assert_eq!(capture.records()[0].field("job"), Some("j1"));
/// ```
#[derive(Clone)]
pub struct CaptureLogger {
    records: Arc<Mutex<Vec<CapturedRecord>>>,
    level: LevelFilter,
}

impl CaptureLogger {
    /// Captures records at every level.
    #[must_use]
    pub fn new() -> CaptureLogger {
        CaptureLogger::with_level(LevelFilter::Trace)
    }

    /// Captures records up to `level`.
    #[must_use]
    pub fn with_level(level: impl Into<LevelFilter>) -> CaptureLogger {
        CaptureLogger {
            records: Arc::new(Mutex::new(Vec::new())),
            level: level.into(),
        }
    }

    /// Runs `f` with this logger receiving everything logged on the current
    /// thread.
    pub fn scope<R>(&self, f: impl FnOnce() -> R) -> R {
        with_logger(Arc::new(self.clone()), f)
    }

    /// A copy of the records captured so far.
    #[must_use]
    pub fn records(&self) -> Vec<CapturedRecord> {
        lock(&self.records).clone()
    }

    /// Removes and returns the records captured so far.
    pub fn take(&self) -> Vec<CapturedRecord> {
        std::mem::take(&mut *lock(&self.records))
    }

    /// Discards the records captured so far.
    pub fn clear(&self) {
        lock(&self.records).clear();
    }

    /// Number of records captured.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.records).len()
    }

    /// Whether nothing has been captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The messages captured so far.
    #[must_use]
    pub fn messages(&self) -> Vec<String> {
        lock(&self.records)
            .iter()
            .map(|r| r.message.clone())
            .collect()
    }

    /// Whether any captured message contains `needle`.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        lock(&self.records)
            .iter()
            .any(|r| r.message.contains(needle))
    }

    /// The first captured record matching `predicate`.
    pub fn find(&self, predicate: impl Fn(&CapturedRecord) -> bool) -> Option<CapturedRecord> {
        lock(&self.records).iter().find(|r| predicate(r)).cloned()
    }
}

impl Default for CaptureLogger {
    fn default() -> Self {
        CaptureLogger::new()
    }
}

impl Logger for CaptureLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.level.allows(metadata.level())
    }

    fn log(&self, record: &Record<'_>) {
        let captured = CapturedRecord {
            level: record.level(),
            target: record.target().to_owned(),
            message: record.message().to_string(),
            fields: record
                .fields()
                .iter()
                .map(|f| (f.key().to_owned(), f.value().to_string()))
                .collect(),
        };
        lock(&self.records).push(captured);
    }
}
