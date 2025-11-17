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

pub(crate) fn check_metric_name(name: &str) -> Result<()> {
    if is_valid_metric_name(name) {
        Ok(())
    } else {
        Err(Error::InvalidMetricName(name.to_owned()))
    }
}

/// Validates the label names of a family of type `kind`, returning them owned
/// and in declaration order (which is also the order they are rendered in).
pub(crate) fn check_label_names(kind: MetricType, names: &[&str]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for &name in names {
        let reason = if !is_valid_label_name(name) {
            Some("must match [a-zA-Z_][a-zA-Z0-9_]*")
        } else if name.starts_with("__") {
            Some("names starting with \"__\" are reserved")
        } else if kind == MetricType::Histogram && name == "le" {
            Some("\"le\" is reserved for histogram buckets")
        } else if kind == MetricType::Summary && name == "quantile" {
            Some("\"quantile\" is reserved for summary quantiles")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(Error::InvalidLabelName {
                name: name.to_owned(),
                reason,
            });
        }
        if out.iter().any(|seen| seen == name) {
            return Err(Error::DuplicateLabel(name.to_owned()));
        }
        out.push(name.to_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_names_follow_the_prometheus_grammar() {
        for good in ["a", "_", ":", "http_requests_total", "job:rate5m", "A1_b2"] {
            assert!(is_valid_metric_name(good), "{good} should be valid");
        }
        for bad in ["", "1a", "a-b", "a b", "a.b", "é", "a\n"] {
            assert!(!is_valid_metric_name(bad), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn label_names_reject_colons_and_leading_digits() {
        assert!(is_valid_label_name("queue"));
        assert!(is_valid_label_name("_private"));
        assert!(!is_valid_label_name("a:b"));
        assert!(!is_valid_label_name("0queue"));
        assert!(!is_valid_label_name(""));
    }

    #[test]
    fn rejects_reserved_double_underscore_labels() {
        let err = check_label_names(MetricType::Counter, &["__name__"]).unwrap_err();
        assert!(matches!(err, Error::InvalidLabelName { ref name, .. } if name == "__name__"));
    }

    #[test]
    fn le_is_reserved_only_for_histograms() {
        assert!(check_label_names(MetricType::Histogram, &["le"]).is_err());
        assert!(check_label_names(MetricType::Gauge, &["le"]).is_ok());
        assert!(check_label_names(MetricType::Summary, &["quantile"]).is_err());
        assert!(check_label_names(MetricType::Histogram, &["quantile"]).is_ok());
    }

    #[test]
    fn rejects_duplicate_label_names() {
        let err = check_label_names(MetricType::Counter, &["queue", "state", "queue"]);
        assert_eq!(err, Err(Error::DuplicateLabel("queue".into())));
    }

    #[test]
    fn keeps_declaration_order() {
        let names = check_label_names(MetricType::Counter, &["z", "a", "m"]).unwrap();
        assert_eq!(names, ["z", "a", "m"]);
    }
}
