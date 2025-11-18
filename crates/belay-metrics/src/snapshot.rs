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

/// A histogram's cumulative buckets. The last bucket's upper bound is always
/// `+Inf` and its count equals [`count`](Self::count).
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramValue {
    /// Buckets in increasing bound order.
    pub buckets: Vec<Bucket>,
    /// Sum of all observations.
    pub sum: f64,
    /// Number of observations.
    pub count: u64,
}

/// One cumulative histogram bucket: observations `<= upper_bound`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    /// Inclusive upper bound (`le`).
    pub upper_bound: f64,
    /// Observations at or below the bound.
    pub cumulative_count: u64,
}

impl HistogramValue {
    /// Mean of the observations, or `None` before the first one.
    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }

    /// Estimates the `q`-quantile the way PromQL's `histogram_quantile` does:
    /// find the bucket holding rank `q * count` and interpolate linearly
    /// inside it, assuming observations are spread evenly across the bucket.
    ///
    /// The lowest bucket is taken to start at 0 when its bound is positive.
    /// A rank that lands in the `+Inf` bucket returns the highest finite
    /// bound. Returns `NaN` with no observations or `q` outside `[0, 1]`.
    ///
    /// ```
    /// use belay_metrics::Histogram;
    ///
    /// let h = Histogram::new(&[1.0, 2.0, 4.0]).unwrap();
    /// for v in [0.5, 1.5, 1.5, 3.0] {
    ///     h.observe(v);
    /// }
    /// // rank 2 of 4 falls halfway through the (1, 2] bucket, which holds 2.
    /// assert_eq!(h.snapshot().quantile(0.5), 1.5);
    /// ```
    pub fn quantile(&self, q: f64) -> f64 {
        if self.count == 0 || !(0.0..=1.0).contains(&q) {
            return f64::NAN;
        }
        let rank = q * self.count as f64;
        let mut lower = 0.0;
        let mut below = 0u64;
        for (i, bucket) in self.buckets.iter().enumerate() {
            if (bucket.cumulative_count as f64) >= rank {
                if bucket.upper_bound.is_infinite() {
                    return if i == 0 { f64::NAN } else { lower };
                }
                if i == 0 && bucket.upper_bound <= 0.0 {
                    return bucket.upper_bound;
                }
                let in_bucket = (bucket.cumulative_count - below) as f64;
                if in_bucket == 0.0 {
                    return lower;
                }
                let fraction = (rank - below as f64) / in_bucket;
                return lower + (bucket.upper_bound - lower) * fraction;
            }
            lower = bucket.upper_bound;
            below = bucket.cumulative_count;
        }
        lower
    }
}
