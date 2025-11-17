//! Point-in-time copies of metric values as plain Rust data.
//!
//! [`Registry::snapshot`](crate::Registry::snapshot) returns these so other
//! exporters (the server's JSON endpoints, the CLI) can present metrics
//! without parsing the text format. [`Collector`](crate::Collector)s also
//! produce them.

use crate::desc::{Desc, MetricType};

/// One metric family with all of its series.
#[derive(Debug, Clone, PartialEq)]
pub struct FamilySnapshot {
    /// Family name.
    pub name: String,
    /// Help text, unescaped.
    pub help: String,
    /// Metric type; every [`MetricSnapshot::value`] has this type.
    pub kind: MetricType,
    /// Series sorted by label values (registry families) or in collector order.
    pub metrics: Vec<MetricSnapshot>,
}

impl FamilySnapshot {
    /// An empty family with the name, help and type of `desc`.
    pub fn new(desc: &Desc) -> Self {
        FamilySnapshot {
            name: desc.name().to_owned(),
            help: desc.help().to_owned(),
            kind: desc.kind(),
            metrics: Vec::new(),
        }
    }

    /// Adds a series; builder-style, for collectors.
    #[must_use]
    pub fn with_metric(mut self, labels: &[(&str, &str)], value: MetricValue) -> Self {
        self.metrics.push(MetricSnapshot {
            labels: labels
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            value,
        });
        self
    }

    /// The series whose labels are exactly `labels`, in any order.
    pub fn metric(&self, labels: &[(&str, &str)]) -> Option<&MetricSnapshot> {
        self.metrics.iter().find(|m| {
            m.labels.len() == labels.len() && labels.iter().all(|&(k, v)| m.label(k) == Some(v))
        })
    }
}

/// One series: its label pairs and value.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSnapshot {
    /// Label pairs in the family's declaration order.
    pub labels: Vec<(String, String)>,
    /// The value.
    pub value: MetricValue,
}

impl MetricSnapshot {
    /// The value of label `name`, if present.
    pub fn label(&self, name: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A series value.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricValue {
    /// Counter value.
    Counter(u64),
    /// Gauge value.
    Gauge(f64),
    /// Histogram buckets, sum and count.
    Histogram(HistogramValue),
    /// Summary quantiles, sum and count.
    Summary(SummaryValue),
}

impl MetricValue {
    /// The metric type this value belongs to.
    pub fn kind(&self) -> MetricType {
        match self {
            MetricValue::Counter(_) => MetricType::Counter,
            MetricValue::Gauge(_) => MetricType::Gauge,
            MetricValue::Histogram(_) => MetricType::Histogram,
            MetricValue::Summary(_) => MetricType::Summary,
        }
    }

    /// The counter value, if this is a counter.
    pub fn as_counter(&self) -> Option<u64> {
        match self {
            MetricValue::Counter(v) => Some(*v),
            _ => None,
        }
    }

    /// The gauge value, if this is a gauge.
    pub fn as_gauge(&self) -> Option<f64> {
        match self {
            MetricValue::Gauge(v) => Some(*v),
            _ => None,
        }
    }

    /// The histogram value, if this is a histogram.
    pub fn as_histogram(&self) -> Option<&HistogramValue> {
        match self {
            MetricValue::Histogram(v) => Some(v),
            _ => None,
        }
    }

    /// The summary value, if this is a summary.
    pub fn as_summary(&self) -> Option<&SummaryValue> {
        match self {
            MetricValue::Summary(v) => Some(v),
            _ => None,
        }
    }
}
