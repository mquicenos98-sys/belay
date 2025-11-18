//! The Prometheus text exposition format, version 0.0.4
//! (<https://prometheus.io/docs/instrumenting/exposition_formats/>).
//!
//! [`Registry::render_prometheus`](crate::Registry::render_prometheus) is the
//! usual entry point; the functions here are public for code that filters or
//! merges snapshots before rendering them.
//!
//! Output rules: `# HELP` (omitted when the help text is empty) and `# TYPE`
//! precede each family; families without series are skipped entirely, as the
//! reference client does; labels appear in declaration order with `le` or
//! `quantile` last; every line ends in `\n`.

use std::borrow::Cow;
use std::fmt::Write as _;

use crate::snapshot::{FamilySnapshot, MetricValue};

/// The `Content-Type` of a text-format scrape response.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Renders `families` in the given order.
pub fn encode(families: &[FamilySnapshot]) -> String {
    let mut out = String::new();
    for family in families {
        encode_family(&mut out, family);
    }
    out
}

/// Appends one family to `out`; writes nothing if it has no series.
pub fn encode_family(out: &mut String, family: &FamilySnapshot) {
    if family.metrics.is_empty() {
        return;
    }
    let name = family.name.as_str();
    if !family.help.is_empty() {
        let _ = writeln!(out, "# HELP {name} {}", escape_help(&family.help));
    }
    let _ = writeln!(out, "# TYPE {name} {}", family.kind);
    for metric in &family.metrics {
        let labels = &metric.labels;
        match &metric.value {
            MetricValue::Counter(v) => {
                sample(out, name, "", labels, None);
                let _ = writeln!(out, "{v}");
            }
            MetricValue::Gauge(v) => {
                sample(out, name, "", labels, None);
                push_float(out, *v);
            }
            MetricValue::Histogram(h) => {
                for bucket in &h.buckets {
                    let le = format_float(bucket.upper_bound);
                    sample(out, name, "_bucket", labels, Some(("le", &le)));
                    let _ = writeln!(out, "{}", bucket.cumulative_count);
                }
                sample(out, name, "_sum", labels, None);
                push_float(out, h.sum);
                sample(out, name, "_count", labels, None);
                let _ = writeln!(out, "{}", h.count);
            }
            MetricValue::Summary(s) => {
                for q in &s.quantiles {
                    let quantile = format_float(q.quantile);
                    sample(out, name, "", labels, Some(("quantile", &quantile)));
                    push_float(out, q.value);
                }
                sample(out, name, "_sum", labels, None);
                push_float(out, s.sum);
                sample(out, name, "_count", labels, None);
                let _ = writeln!(out, "{}", s.count);
            }
        }
    }
}

/// Writes `name{labels} ` (everything up to the value).
fn sample(
    out: &mut String,
    name: &str,
    suffix: &str,
    labels: &[(String, String)],
    extra: Option<(&str, &str)>,
) {
    out.push_str(name);
    out.push_str(suffix);
    let extra = extra.into_iter();
    let mut pairs = labels
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .chain(extra)
        .peekable();
    if pairs.peek().is_some() {
        out.push('{');
        for (i, (k, v)) in pairs.enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(k);
            out.push_str("=\"");
            out.push_str(&escape_label_value(v));
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
}

fn push_float(out: &mut String, v: f64) {
    out.push_str(&format_float(v));
    out.push('\n');
}

/// Formats a sample value or `le`/`quantile` label the way Prometheus parses
/// it: `NaN`, `+Inf`, `-Inf`; integral values as plain digits (`1`, not
/// `1.0` or `1e0`); other values in the shortest form that round-trips,
/// switching to exponent notation only outside `[1e-5, 1e21)` so huge or tiny
/// magnitudes stay short. Negative zero renders as `0`.
///
/// ```
/// use belay_metrics::text::format_float;
///
/// assert_eq!(format_float(1.0), "1");
/// assert_eq!(format_float(2500000.0), "2500000");
/// assert_eq!(format_float(0.25), "0.25");
/// assert_eq!(format_float(f64::INFINITY), "+Inf");
/// assert_eq!(format_float(1.5e-9), "1.5e-9");
/// ```
pub fn format_float(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_owned();
    }
    if v.is_infinite() {
        return if v > 0.0 { "+Inf" } else { "-Inf" }.to_owned();
    }
    if v == 0.0 {
        return "0".to_owned();
    }
    // `{:e}` gives the shortest round-trip digits with an exact decimal
    // exponent, which avoids guessing the magnitude with log10.
    let scientific = format!("{v:e}");
    let exponent: i32 = scientific
        .rsplit_once('e')
        .and_then(|(_, e)| e.parse().ok())
        .unwrap_or(0);
    if (-5..21).contains(&exponent) {
        // Display is also shortest round-trip, never uses an exponent and
        // prints integral values without a fractional part.
        v.to_string()
    } else {
        scientific
    }
}

/// Escapes a label value: backslash, double quote and line feed.
///
/// ```
/// use belay_metrics::text::escape_label_value;
///
/// assert_eq!(escape_label_value(r#"C:\q "1""#), r#"C:\\q \"1\""#);
/// assert_eq!(escape_label_value("plain"), "plain");
/// ```
pub fn escape_label_value(value: &str) -> Cow<'_, str> {
    escape(value, true)
}

/// Escapes help text: backslash and line feed (quotes are left alone).
pub fn escape_help(help: &str) -> Cow<'_, str> {
    escape(help, false)
}
