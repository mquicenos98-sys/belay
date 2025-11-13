//! Counters, gauges, histograms and summaries collected in a [`Registry`]
//! and rendered in the Prometheus text exposition format (version 0.0.4).
//!
//! A registry holds *families*: a name, help text, type and fixed list of
//! label names. A family without labels has a single series and the registry
//! hands out its handle directly; a family with labels is a *vec*
//! ([`CounterVec`], [`GaugeVec`], [`HistogramVec`], [`SummaryVec`]) whose
//! children are looked up by label values.
//!
//! ```
//! use belay_metrics::{Registry, DEFAULT_BUCKETS};
//!
//! # fn main() -> belay_metrics::Result<()> {
//! let registry = Registry::new();
//! let enqueued = registry.counter_vec(
//!     "belay_jobs_enqueued_total",
//!     "Jobs accepted by the queue.",
//!     &["queue"],
//! )?;
//! let lease_latency = registry.histogram(
//!     "belay_lease_seconds",
//!     "Time from lease request to response.",
//!     &DEFAULT_BUCKETS,
//! )?;
//!
//! enqueued.with_label_values(&["emails"]).inc_by(3);
//! lease_latency.observe(0.042);
//!
//! let text = registry.render_prometheus();
//! assert!(text.contains("belay_jobs_enqueued_total{queue=\"emails\"} 3\n"));
//! assert!(text.contains("belay_lease_seconds_bucket{le=\"0.05\"} 1\n"));
//! # Ok(())
//! # }
//! ```
//!
//! # The hot path
//!
//! Handles are cheap to clone and share their series. Updating one
//! (`inc`, `set`, `observe`) is a few atomic operations and never takes a
//! lock, summaries included. Finding a child in a vec takes a read lock and
//! hashes the label values, so look children up once and keep the handle.
//!
//! # Registration is idempotent
//!
//! Registering a name again with an identical definition returns the
//! existing family; any difference is an error. See [`Registry`] for the
//! exact rules, including the check that no two families expose the same
//! series name.
//!
//! # Exporting elsewhere
//!
//! [`Registry::snapshot`] returns plain structs ([`FamilySnapshot`] and
//! friends) for JSON or table output. Values computed at scrape time come
//! from a [`Collector`], such as the built-in [`ProcessCollector`], or from
//! [`Registry::register_gauge_fn`].
//!
//! # Limitations
//!
//! Only the text format is produced (no protobuf, no OpenMetrics
//! exemplars). There are no const labels; put fixed labels such as a node id
//! in the vec's labels. Counters are integers; fractional totals such as CPU
//! seconds need a gauge or a collector.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod atomic;
mod counter;
mod desc;
mod error;
mod gauge;
mod histogram;
mod names;
mod process;
mod registry;
mod snapshot;
mod summary;
pub mod text;
mod time;
mod timer;
mod vec;

pub use counter::Counter;
pub use desc::{Desc, MetricType};
pub use error::{Error, Result};
pub use gauge::{Gauge, InFlight};
pub use histogram::{exponential_buckets, linear_buckets, Histogram, DEFAULT_BUCKETS};
pub use names::{is_valid_label_name, is_valid_metric_name};
pub use process::ProcessCollector;
pub use registry::{Collector, Registry};
pub use snapshot::{
    Bucket, FamilySnapshot, HistogramValue, MetricSnapshot, MetricValue, Quantile, SummaryValue,
};
pub use summary::{Summary, SummaryOpts};
pub use time::{ManualTime, MonotonicTime, SharedTimeSource, TimeSource};
pub use timer::{HistogramTimer, Observe, SummaryTimer, Timer};
pub use vec::{CounterVec, GaugeVec, HistogramVec, SummaryVec};

/// Lock helpers. A panic while a registry lock is held cannot leave the
/// protected maps half-updated (every critical section is a single insert,
/// remove or read), so poisoning is ignored rather than propagated.
mod sync {
    use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

    pub(crate) fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
        lock.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
        lock.write().unwrap_or_else(PoisonError::into_inner)
    }
}
