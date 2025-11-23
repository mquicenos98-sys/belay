//! Registration semantics: idempotency, conflicts, collisions, collectors.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use belay_metrics::{
    Collector, Desc, Error, FamilySnapshot, MetricType, MetricValue, Registry, Result, SummaryOpts,
    DEFAULT_BUCKETS,
};

#[test]
fn same_definition_returns_the_existing_family() -> Result<()> {
    let registry = Registry::new();
    let a = registry.counter_vec("jobs_total", "Jobs.", &["queue"])?;
    let b = registry.counter_vec("jobs_total", "Jobs.", &["queue"])?;
    a.with_label_values(&["q"]).inc();
    assert_eq!(b.with_label_values(&["q"]).get(), 1);

    let h1 = registry.histogram("lat_seconds", "Latency.", &DEFAULT_BUCKETS)?;
    let h2 = registry.histogram("lat_seconds", "Latency.", &DEFAULT_BUCKETS)?;
    h1.observe(0.2);
    assert_eq!(h2.count(), 1);

    let s1 = registry.summary("run_seconds", "Run.", SummaryOpts::new())?;
    let s2 = registry.summary("run_seconds", "Run.", SummaryOpts::new())?;
    s1.observe(1.0);
    assert_eq!(s2.count(), 1);
    Ok(())
}

#[test]
fn unlabelled_constructor_and_empty_vec_are_the_same_family() -> Result<()> {
    let registry = Registry::new();
    let vec = registry.gauge_vec("temperature", "", &[])?;
    let gauge = registry.gauge("temperature", "")?;
    gauge.set(21.5);
    assert_eq!(vec.with_label_values(&[]).get(), 21.5);
    Ok(())
}

#[test]
fn summary_options_are_compared_after_normalisation() -> Result<()> {
    let registry = Registry::new();
    let a = registry.summary("s", "", SummaryOpts::new().quantiles(&[0.9, 0.5]))?;
    let b = registry.summary("s", "", SummaryOpts::new().quantiles(&[0.5, 0.9, 0.9]))?;
    a.observe(3.0);
    assert_eq!(b.count(), 1);
    Ok(())
}

#[test]
fn different_definitions_conflict() {
    let registry = Registry::new();
    registry.counter_vec("x", "help", &["a"]).unwrap();

    let conflicts = [
        registry.gauge_vec("x", "help", &["a"]).map(drop),
        registry.counter_vec("x", "help", &["b"]).map(drop),
        registry.counter_vec("x", "help", &["a", "b"]).map(drop),
        registry.counter("x", "help").map(drop),
        registry.counter_vec("x", "other help", &["a"]).map(drop),
    ];
    for result in conflicts {
        assert!(
            matches!(result, Err(Error::Conflict { ref name, .. }) if name == "x"),
            "{result:?}"
        );
    }

    let err = registry.gauge_vec("x", "help", &["a"]).unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"metric "x" is already registered as a counter; cannot register it as a gauge"#
    );
}

#[test]
fn summary_with_other_options_conflicts() -> Result<()> {
    let registry = Registry::new();
    registry.summary("s", "", SummaryOpts::new())?;
    let err = registry
        .summary("s", "", SummaryOpts::new().max_age(Duration::from_secs(60)))
        .unwrap_err();
    assert!(err.to_string().ends_with("different summary options"));
    Ok(())
}

#[test]
fn suffixed_series_names_collide() {
    let registry = Registry::new();
    registry.histogram("rpc", "", &[1.0]).unwrap();

    for name in ["rpc_bucket", "rpc_sum", "rpc_count"] {
        let err = registry.gauge(name, "").unwrap_err();
        assert_eq!(
            err,
            Error::Collision {
                name: name.into(),
                series: name.into(),
                owner: "rpc".into()
            }
        );
    }
    // The other direction: an existing gauge blocks a summary's suffix.
    registry.gauge("lag_sum", "").unwrap();
    assert!(matches!(
        registry.summary("lag", "", SummaryOpts::new()),
        Err(Error::Collision { ref owner, .. }) if owner == "lag_sum"
    ));
    // `_total` is not a reserved suffix.
    registry.counter("rpc_total", "").unwrap();
}

#[test]
fn failed_registration_leaves_no_trace() {
    let registry = Registry::new();
    assert!(matches!(
        registry.counter("bad-name", ""),
        Err(Error::InvalidMetricName(_))
    ));
    assert!(matches!(
        registry.histogram_vec("h", "", &["le"], &[1.0]),
        Err(Error::InvalidLabelName { .. })
    ));
    assert!(matches!(
        registry.histogram("h", "", &[2.0, 1.0]),
        Err(Error::InvalidBuckets(_))
    ));
    assert!(matches!(
        registry.summary("s", "", SummaryOpts::new().age_buckets(0)),
        Err(Error::InvalidSummary(_))
    ));
    assert!(!registry.contains("h"));
    assert!(!registry.contains("s"));
    assert!(registry.snapshot().is_empty());
}

#[test]
fn unregister_frees_the_name_and_detaches_handles() -> Result<()> {
    let registry = Registry::new();
    let old = registry.counter("c", "")?;
    old.inc();
    assert!(registry.unregister("c"));
    assert!(!registry.unregister("c"));
    assert_eq!(registry.render_prometheus(), "");

    let fresh = registry.gauge("c", "now a gauge")?;
    old.inc();
    fresh.set(7.0);
    assert_eq!(
        registry.render_prometheus(),
        "# HELP c now a gauge\n# TYPE c gauge\nc 7\n"
    );
    Ok(())
}

#[test]
fn clones_share_families() -> Result<()> {
    let registry = Registry::new();
    let clone = registry.clone();
    clone.counter("c", "")?.inc_by(5);
    let family = &registry.snapshot()[0];
    assert_eq!(family.metrics[0].value.as_counter(), Some(5));
    assert_eq!(
        format!("{registry:?}"),
        r#"Registry { families: ["c"], collectors: 0 }"#
    );
    Ok(())
}

#[test]
fn snapshot_includes_empty_families_in_name_order() -> Result<()> {
    let registry = Registry::new();
    registry.gauge_vec("b", "", &["x"])?;
    registry.counter("a", "A.")?;
    let snapshot = registry.snapshot();
    let names: Vec<&str> = snapshot.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(snapshot[0].help, "A.");
    assert_eq!(snapshot[0].kind, MetricType::Counter);
    assert!(snapshot[1].metrics.is_empty());
    Ok(())
}

struct Fixed {
    descs: Vec<Desc>,
}

impl Fixed {
    fn new(names: &[&str]) -> Arc<Fixed> {
        let descs = names
            .iter()
            .map(|n| Desc::new(n, "", MetricType::Gauge, &[]).unwrap())
            .collect();
        Arc::new(Fixed { descs })
    }
}

impl Collector for Fixed {
    fn describe(&self) -> Vec<Desc> {
        self.descs.clone()
    }

    fn collect(&self) -> Vec<FamilySnapshot> {
        self.descs
            .iter()
            .map(|d| FamilySnapshot::new(d).with_metric(&[], MetricValue::Gauge(1.0)))
            .collect()
    }
}

#[test]
fn collector_names_are_checked_against_everything() -> Result<()> {
    let registry = Registry::new();
    registry.histogram("rpc", "", &[1.0])?;
    registry.register_collector(Fixed::new(&["disk_bytes"]))?;

    assert!(matches!(
        registry.register_collector(Fixed::new(&["rpc"])),
        Err(Error::Conflict { ref detail, .. }) if detail == "is already registered"
    ));
    assert!(matches!(
        registry.register_collector(Fixed::new(&["rpc_count"])),
        Err(Error::Collision { .. })
    ));
    assert!(matches!(
        registry.register_collector(Fixed::new(&["disk_bytes"])),
        Err(Error::Conflict { ref detail, .. }) if detail.contains("collector")
    ));
    assert!(matches!(
        registry.register_collector(Fixed::new(&["a", "b", "a"])),
        Err(Error::Conflict { .. })
    ));
    // Families registered later are checked against collectors too.
    assert!(matches!(
        registry.gauge("disk_bytes", ""),
        Err(Error::Conflict { .. })
    ));
    // Nothing from the rejected collectors was kept.
    let names: Vec<String> = registry.snapshot().into_iter().map(|f| f.name).collect();
    assert_eq!(names, ["disk_bytes", "rpc"]);
    Ok(())
}

#[test]
fn collector_output_of_the_wrong_type_is_dropped() -> Result<()> {
    struct Liar;
    impl Collector for Liar {
        fn describe(&self) -> Vec<Desc> {
            vec![Desc::new("liar", "", MetricType::Gauge, &[]).unwrap()]
        }
        fn collect(&self) -> Vec<FamilySnapshot> {
            let as_counter = Desc::new("liar", "", MetricType::Counter, &[]).unwrap();
            vec![FamilySnapshot::new(&as_counter).with_metric(&[], MetricValue::Counter(1))]
        }
    }
    let registry = Registry::new();
    registry.register_collector(Arc::new(Liar))?;
    assert!(registry.snapshot().is_empty());
    Ok(())
}

#[test]
fn unregister_collector_uses_identity() -> Result<()> {
    let registry = Registry::new();
    let collector: Arc<dyn Collector> = Fixed::new(&["g"]);
    registry.register_collector(Arc::clone(&collector))?;
    let lookalike: Arc<dyn Collector> = Fixed::new(&["g"]);
    assert!(!registry.unregister_collector(&lookalike));
    assert!(registry.unregister_collector(&collector));
    assert!(registry.snapshot().is_empty());
    // The name is free again.
    registry.gauge("g", "")?;
    Ok(())
}

#[test]
fn function_metrics_are_read_at_scrape_time() -> Result<()> {
    let registry = Registry::new();
    let writes = Arc::new(AtomicU64::new(0));
    let source = Arc::clone(&writes);
    registry.register_counter_fn("store_writes_total", "WAL writes.", move || {
        source.load(Ordering::Relaxed)
    })?;
    registry.register_gauge_fn("store_ratio", "", || 0.5)?;

    writes.store(17, Ordering::Relaxed);
    assert_eq!(
        registry.render_prometheus(),
        "# TYPE store_ratio gauge\nstore_ratio 0.5\n\
         # HELP store_writes_total WAL writes.\n# TYPE store_writes_total counter\nstore_writes_total 17\n"
    );
    assert!(matches!(
        registry.register_gauge_fn("store_ratio", "", || 1.0),
        Err(Error::Conflict { .. })
    ));
    assert!(matches!(
        registry.register_gauge_fn("bad name", "", || 1.0),
        Err(Error::InvalidMetricName(_))
    ));
    Ok(())
}

#[test]
fn vec_lookup_errors_carry_detail() -> Result<()> {
    let registry = Registry::new();
    let vec = registry.histogram_vec("h", "", &["queue", "kind"], &[1.0])?;
    assert_eq!(
        vec.try_with_label_values(&["a"]).unwrap_err().to_string(),
        "expected 2 label values, got 1"
    );
    assert_eq!(
        vec.try_with(&[("queue", "a"), ("knd", "b")])
            .unwrap_err()
            .to_string(),
        r#"unknown label "knd""#
    );
    assert_eq!(vec.buckets(), [1.0]);
    assert_eq!(vec.desc().label_names(), ["queue", "kind"]);
    Ok(())
}
