//! Metric family descriptors: name, help text, type and label names.

use std::fmt;

use crate::error::Result;
use crate::names::{check_label_names, check_metric_name};

/// The four metric types of the Prometheus data model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MetricType {
    /// A monotonically increasing integer.
    Counter,
    /// A value that can go up and down.
    Gauge,
    /// Observations counted into cumulative buckets, plus their sum and count.
    Histogram,
    /// Observations summarised as quantiles over a sliding window, plus their
    /// sum and count.
    Summary,
}

impl MetricType {
    /// The lowercase name used in `# TYPE` lines.
    pub fn as_str(self) -> &'static str {
        match self {
            MetricType::Counter => "counter",
            MetricType::Gauge => "gauge",
            MetricType::Histogram => "histogram",
            MetricType::Summary => "summary",
        }
    }
}
