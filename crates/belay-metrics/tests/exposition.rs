//! Golden tests of the Prometheus text format as rendered by a registry.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use belay_metrics::{
    text, Collector, Desc, FamilySnapshot, ManualTime, MetricType, MetricValue, ProcessCollector,
    Registry, Result, SummaryOpts,
};

fn manual_registry() -> (ManualTime, Registry) {
    let time = ManualTime::new();
    let registry = Registry::with_time_source(Arc::new(time.clone()));
    (time, registry)
}

#[test]
fn renders_every_metric_type_sorted_by_family() -> Result<()> {
    let (_, registry) = manual_registry();

    registry.gauge("belay_up", "")?.set(1.0);

    let enqueued = registry.counter_vec(
        "belay_jobs_enqueued_total",
        "Jobs accepted by the queue.",
        &["queue"],
    )?;
    enqueued.with_label_values(&["reports"]).inc();
    enqueued.with_label_values(&["emails"]).inc_by(41);
    enqueued.with_label_values(&["emails"]).inc();

    let depth = registry.gauge_vec(
        "belay_queue_depth",
        "Jobs waiting, by queue and state.",
        &["queue", "state"],
    )?;
    depth.with_label_values(&["emails", "scheduled"]).set(1.5);
    depth.with_label_values(&["emails", "ready"]).set(12.0);

    let lease = registry.histogram_vec(
        "belay_lease_seconds",
        "Lease wait.\nMeasured server-side.",
        &["queue"],
        &[0.005, 0.1, 1.0],
    )?;
    let emails = lease.with_label_values(&["emails"]);
    // Binary fractions keep the sum exact.
    for v in [0.001953125, 0.0625, 0.0625, 0.5, 2.0] {
        emails.observe(v);
    }

    let duration = registry.summary(
        "belay_job_duration_seconds",
        "Handler run time.",
        SummaryOpts::new().quantiles(&[0.9, 0.5]),
    )?;
    for v in 1..=10 {
        duration.observe(f64::from(v));
    }

    // Declared but never used: no series, so no output.
    registry.counter_vec("belay_unused_total", "Never touched.", &["x"])?;

    let expected = r#"# HELP belay_job_duration_seconds Handler run time.
# TYPE belay_job_duration_seconds summary
belay_job_duration_seconds{quantile="0.5"} 5
belay_job_duration_seconds{quantile="0.9"} 9
belay_job_duration_seconds_sum 55
belay_job_duration_seconds_count 10
# HELP belay_jobs_enqueued_total Jobs accepted by the queue.
# TYPE belay_jobs_enqueued_total counter
belay_jobs_enqueued_total{queue="emails"} 42
belay_jobs_enqueued_total{queue="reports"} 1
# HELP belay_lease_seconds Lease wait.\nMeasured server-side.
# TYPE belay_lease_seconds histogram
belay_lease_seconds_bucket{queue="emails",le="0.005"} 1
belay_lease_seconds_bucket{queue="emails",le="0.1"} 3
belay_lease_seconds_bucket{queue="emails",le="1"} 4
belay_lease_seconds_bucket{queue="emails",le="+Inf"} 5
belay_lease_seconds_sum{queue="emails"} 2.626953125
belay_lease_seconds_count{queue="emails"} 5
# HELP belay_queue_depth Jobs waiting, by queue and state.
# TYPE belay_queue_depth gauge
belay_queue_depth{queue="emails",state="ready"} 12
belay_queue_depth{queue="emails",state="scheduled"} 1.5
# TYPE belay_up gauge
belay_up 1
"#;
    assert_eq!(registry.render_prometheus(), expected);
    Ok(())
}

#[test]
fn escapes_label_values_and_help() -> Result<()> {
    let registry = Registry::new();
    let bytes = registry.gauge_vec("path_bytes", r#"Bytes "by" path, see C:\docs"#, &["path"])?;
    bytes.with_label_values(&[r#"C:\data\"q""#]).set(1.0);
    bytes.with_label_values(&["line1\nline2"]).set(2.0);
    bytes.with_label_values(&["plain ünïcödé"]).set(3.0);

    let expected = r#"# HELP path_bytes Bytes "by" path, see C:\\docs
# TYPE path_bytes gauge
path_bytes{path="C:\\data\\\"q\""} 1
path_bytes{path="line1\nline2"} 2
path_bytes{path="plain ünïcödé"} 3
"#;
    assert_eq!(registry.render_prometheus(), expected);
    Ok(())
}
