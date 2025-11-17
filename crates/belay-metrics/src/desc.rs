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

impl fmt::Display for MetricType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The validated definition of a metric family.
///
/// Registration compares descriptors to decide whether a second registration
/// of a name is the same family (and returns it) or a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desc {
    name: String,
    help: String,
    kind: MetricType,
    label_names: Vec<String>,
}

impl Desc {
    /// Validates and builds a descriptor.
    ///
    /// Fails if `name` is not a valid metric name, or if a label name is
    /// malformed, reserved (`__` prefix; `le` on histograms; `quantile` on
    /// summaries) or repeated. Help text may be anything, including empty.
    pub fn new(name: &str, help: &str, kind: MetricType, label_names: &[&str]) -> Result<Desc> {
        check_metric_name(name)?;
        let label_names = check_label_names(kind, label_names)?;
        Ok(Desc {
            name: name.to_owned(),
            help: help.to_owned(),
            kind,
            label_names,
        })
    }

    /// The family name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The help text, unescaped.
    pub fn help(&self) -> &str {
        &self.help
    }

    /// The metric type.
    pub fn kind(&self) -> MetricType {
        self.kind
    }

    /// Label names in declaration order.
    pub fn label_names(&self) -> &[String] {
        &self.label_names
    }

    /// Every name this family occupies in an exposition: its own name (used
    /// by `# HELP`/`# TYPE`) and the names that start its sample lines. Two
    /// families whose sets intersect cannot be exposed side by side without
    /// a scraper misattributing samples.
    pub(crate) fn claimed_names(&self) -> Vec<String> {
        let mut names = vec![self.name.clone()];
        let suffixes: &[&str] = match self.kind {
            MetricType::Counter | MetricType::Gauge => &[],
            MetricType::Histogram => &["_bucket", "_sum", "_count"],
            MetricType::Summary => &["_sum", "_count"],
        };
        names.extend(suffixes.iter().map(|s| format!("{}{s}", self.name)));
        names
    }

    /// Explains how `other`, a definition for the same name, differs from
    /// this one. Phrased to follow `metric "<name>"` in an error message.
    pub(crate) fn conflict_detail(&self, other: &Desc) -> String {
        if self.kind != other.kind {
            format!(
                "is already registered as a {}; cannot register it as a {}",
                self.kind, other.kind
            )
        } else if self.label_names != other.label_names {
            format!(
                "is already registered with labels [{}]; cannot register it with labels [{}]",
                self.label_names.join(", "),
                other.label_names.join(", ")
            )
        } else if self.help != other.help {
            format!(
                "is already registered with help text {:?}; cannot register it with {:?}",
                self.help, other.help
            )
        } else {
            match self.kind {
                MetricType::Histogram => {
                    "is already registered with a different bucket layout".to_owned()
                }
                MetricType::Summary => {
                    "is already registered with different summary options".to_owned()
                }
                MetricType::Counter | MetricType::Gauge => {
                    "is already registered with a different definition".to_owned()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn validates_name_and_labels() {
        assert!(matches!(
            Desc::new("bad name", "", MetricType::Gauge, &[]),
            Err(Error::InvalidMetricName(_))
        ));
        assert!(matches!(
            Desc::new("latency_seconds", "", MetricType::Histogram, &["le"]),
            Err(Error::InvalidLabelName { .. })
        ));
        let desc = Desc::new("jobs_total", "Jobs.", MetricType::Counter, &["queue"]).unwrap();
        assert_eq!(desc.name(), "jobs_total");
        assert_eq!(desc.help(), "Jobs.");
        assert_eq!(desc.kind(), MetricType::Counter);
        assert_eq!(desc.label_names(), ["queue"]);
    }

    #[test]
    fn histograms_and_summaries_claim_their_suffixed_series() {
        let h = Desc::new("rpc", "", MetricType::Histogram, &[]).unwrap();
        assert_eq!(
            h.claimed_names(),
            ["rpc", "rpc_bucket", "rpc_sum", "rpc_count"]
        );
        let s = Desc::new("rpc", "", MetricType::Summary, &[]).unwrap();
        assert_eq!(s.claimed_names(), ["rpc", "rpc_sum", "rpc_count"]);
        let c = Desc::new("rpc_total", "", MetricType::Counter, &[]).unwrap();
        assert_eq!(c.claimed_names(), ["rpc_total"]);
    }

    #[test]
    fn conflict_detail_names_the_first_difference() {
        let base = Desc::new("x", "help", MetricType::Counter, &["a"]).unwrap();
        let gauge = Desc::new("x", "help", MetricType::Gauge, &["a"]).unwrap();
        let relabeled = Desc::new("x", "help", MetricType::Counter, &["a", "b"]).unwrap();
        let rehelped = Desc::new("x", "other", MetricType::Counter, &["a"]).unwrap();
        assert_eq!(
            base.conflict_detail(&gauge),
            "is already registered as a counter; cannot register it as a gauge"
        );
        assert_eq!(
            base.conflict_detail(&relabeled),
            "is already registered with labels [a]; cannot register it with labels [a, b]"
        );
        assert!(base.conflict_detail(&rehelped).contains("help text"));
    }
}
