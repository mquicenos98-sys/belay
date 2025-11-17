//! Metric and label name rules from the Prometheus data model
//! (<https://prometheus.io/docs/concepts/data_model/>).
//!
//! Label *values* may be any UTF-8 string and are only escaped on output;
//! see [`text::escape_label_value`](crate::text::escape_label_value).

use crate::desc::MetricType;
use crate::error::{Error, Result};

/// Returns whether `name` is a valid metric name: `[a-zA-Z_:][a-zA-Z0-9_:]*`.
///
/// Colons are legal but conventionally reserved for recording rules, so
/// instrumented code should not use them.
///
/// ```
/// use belay_metrics::is_valid_metric_name;
///
/// assert!(is_valid_metric_name("belay_jobs_enqueued_total"));
/// assert!(!is_valid_metric_name("jobs-enqueued"));
/// assert!(!is_valid_metric_name(""));
/// ```
pub fn is_valid_metric_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' || b == b':' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':')
}

/// Returns whether `name` is a syntactically valid label name:
/// `[a-zA-Z_][a-zA-Z0-9_]*`.
///
/// Syntax is not the whole story: names starting with `__` are reserved, and
/// histograms and summaries reserve `le` and `quantile`. Registration checks
/// those too.
pub fn is_valid_label_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
