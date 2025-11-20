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

#[test]
fn renders_special_and_extreme_floats() -> Result<()> {
    let registry = Registry::new();
    let g = registry.gauge_vec("values", "", &["case"])?;
    for (case, v) in [
        ("a_nan", f64::NAN),
        ("b_pos_inf", f64::INFINITY),
        ("c_neg_inf", f64::NEG_INFINITY),
        ("d_neg_zero", -0.0),
        ("e_big", 1e21),
        ("f_tiny", 1e-7),
        ("g_int", 1_234_567_890.0),
        ("h_negative", -0.125),
    ] {
        g.with_label_values(&[case]).set(v);
    }
    let expected = r#"# TYPE values gauge
values{case="a_nan"} NaN
values{case="b_pos_inf"} +Inf
values{case="c_neg_inf"} -Inf
values{case="d_neg_zero"} 0
values{case="e_big"} 1e21
values{case="f_tiny"} 1e-7
values{case="g_int"} 1234567890
values{case="h_negative"} -0.125
"#;
    assert_eq!(registry.render_prometheus(), expected);
    Ok(())
}

#[test]
fn summary_quantiles_follow_the_injected_clock() -> Result<()> {
    let (time, registry) = manual_registry();
    let opts = SummaryOpts::new()
        .quantiles(&[0.5, 0.99])
        .max_age(Duration::from_secs(60))
        .age_buckets(3);
    let latency = registry.summary_vec("rpc_seconds", "RPC latency.", &["method"], opts)?;
    let get = latency.with_label_values(&["get"]);
    for ms in [10u32, 20, 30, 40] {
        get.observe(f64::from(ms) / 1000.0);
    }
    time.advance(Duration::from_secs(20));
    get.observe(0.5);

    let rendered = registry.render_prometheus();
    assert_eq!(
        rendered,
        r#"# HELP rpc_seconds RPC latency.
# TYPE rpc_seconds summary
rpc_seconds{method="get",quantile="0.5"} 0.03
rpc_seconds{method="get",quantile="0.99"} 0.5
rpc_seconds_sum{method="get"} 0.6
rpc_seconds_count{method="get"} 5
"#
    );

    // The first sub-window (0-20s) expires at 60s; only 0.5 remains.
    time.set(Duration::from_secs(60));
    assert!(registry
        .render_prometheus()
        .contains("rpc_seconds{method=\"get\",quantile=\"0.5\"} 0.5\n"));

    // Everything expired: quantiles are NaN, totals are lifetime values.
    time.set(Duration::from_secs(80));
    let rendered = registry.render_prometheus();
    assert!(rendered.contains("rpc_seconds{method=\"get\",quantile=\"0.99\"} NaN\n"));
    assert!(rendered.contains("rpc_seconds_count{method=\"get\"} 5\n"));
    Ok(())
}

#[test]
fn histogram_timer_lands_in_the_expected_bucket() -> Result<()> {
    let (time, registry) = manual_registry();
    let h = registry.histogram("step_seconds", "Step time.", &[0.1, 0.5])?;
    let timer = h.start_timer();
    time.advance(Duration::from_millis(250));
    drop(timer);
    assert_eq!(
        registry.render_prometheus(),
        "# HELP step_seconds Step time.\n# TYPE step_seconds histogram\n\
         step_seconds_bucket{le=\"0.1\"} 0\nstep_seconds_bucket{le=\"0.5\"} 1\n\
         step_seconds_bucket{le=\"+Inf\"} 1\nstep_seconds_sum 0.25\nstep_seconds_count 1\n"
    );
    Ok(())
}

/// A collector producing families out of order, split in two, and with an
/// undeclared extra that must be dropped.
struct Shards;

impl Collector for Shards {
    fn describe(&self) -> Vec<Desc> {
        vec![Desc::new(
            "shard_rows",
            "Rows per shard.",
            MetricType::Gauge,
            &["shard"],
        )
        .unwrap()]
    }

    fn collect(&self) -> Vec<FamilySnapshot> {
        let desc = &self.describe()[0];
        let undeclared = Desc::new("aaa_sneaky", "", MetricType::Gauge, &[]).unwrap();
        vec![
            FamilySnapshot::new(desc).with_metric(&[("shard", "1")], MetricValue::Gauge(10.0)),
            FamilySnapshot::new(&undeclared).with_metric(&[], MetricValue::Gauge(1.0)),
            FamilySnapshot::new(desc).with_metric(&[("shard", "2")], MetricValue::Gauge(20.0)),
        ]
    }
}

#[test]
fn collector_families_are_merged_filtered_and_sorted_in() -> Result<()> {
    let registry = Registry::new();
    registry.counter("zzz_total", "Last.")?.inc();
    registry.register_collector(Arc::new(Shards))?;
    registry.counter("aab_total", "First.")?;
    let expected = r#"# HELP aab_total First.
# TYPE aab_total counter
aab_total 0
# HELP shard_rows Rows per shard.
# TYPE shard_rows gauge
shard_rows{shard="1"} 10
shard_rows{shard="2"} 20
# HELP zzz_total Last.
# TYPE zzz_total counter
zzz_total 1
"#;
    assert_eq!(registry.render_prometheus(), expected);
    Ok(())
}

/// A throwaway directory shaped like `/proc/self`.
struct FakeProc(PathBuf);

impl FakeProc {
    fn new(tag: &str) -> FakeProc {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "belay-metrics-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("fd")).unwrap();
        FakeProc(dir)
    }
}

impl Drop for FakeProc {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn process_collector_reads_a_procfs_fixture() -> Result<()> {
    let proc = FakeProc::new("proc");
    fs::write(
        proc.0.join("status"),
        "Name:\tbelay\nVmSize:\t  204800 kB\nVmRSS:\t   10240 kB\nThreads:\t7\n",
    )
    .unwrap();
    fs::write(
        proc.0.join("limits"),
        "Limit                     Soft Limit           Hard Limit           Units\n\
         Max open files            4096                 4096                 files\n",
    )
    .unwrap();
    for fd in ["0", "1", "2"] {
        fs::write(proc.0.join("fd").join(fd), "").unwrap();
    }

    let time = ManualTime::starting_at(Duration::from_secs(100));
    let collector = ProcessCollector::with_time_source(Arc::new(time.clone()))
        .proc_dir(Some(&proc.0))
        .start_time(UNIX_EPOCH + Duration::from_millis(1_700_000_000_250));
    let registry = Registry::new();
    registry.register_collector(Arc::new(collector))?;
    time.advance(Duration::from_millis(90_500));

    let expected = r#"# HELP process_max_fds Maximum number of open file descriptors.
# TYPE process_max_fds gauge
process_max_fds 4096
# HELP process_open_fds Number of open file descriptors.
# TYPE process_open_fds gauge
process_open_fds 3
# HELP process_resident_memory_bytes Resident memory size in bytes.
# TYPE process_resident_memory_bytes gauge
process_resident_memory_bytes 10485760
# HELP process_start_time_seconds Start time of the process since the Unix epoch in seconds.
# TYPE process_start_time_seconds gauge
process_start_time_seconds 1700000000.25
# HELP process_threads Number of OS threads in the process.
# TYPE process_threads gauge
process_threads 7
# HELP process_uptime_seconds Seconds since the process started.
# TYPE process_uptime_seconds gauge
process_uptime_seconds 90.5
# HELP process_virtual_memory_bytes Virtual memory size in bytes.
# TYPE process_virtual_memory_bytes gauge
process_virtual_memory_bytes 209715200
"#;
    assert_eq!(registry.render_prometheus(), expected);
    Ok(())
}

#[test]
fn encode_renders_snapshots_in_the_given_order() -> Result<()> {
    let registry = Registry::new();
    registry.counter("b_total", "")?.inc();
    registry.counter("a_total", "")?.inc_by(2);
    let mut families = registry.snapshot();
    families.reverse();
    assert_eq!(
        text::encode(&families),
        "# TYPE b_total counter\nb_total 1\n# TYPE a_total counter\na_total 2\n"
    );
    assert_eq!(
        text::CONTENT_TYPE,
        "text/plain; version=0.0.4; charset=utf-8"
    );
    Ok(())
}
