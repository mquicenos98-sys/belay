//! Labelled families ("vecs"): one child series per distinct tuple of label
//! values, created on first use.
//!
//! Children are found by hashing the label values and comparing them in
//! place, so a lookup allocates nothing; only the first use of a tuple takes
//! the write lock. The lookup still takes a read lock, so hot code should
//! resolve its child once and keep the handle:
//!
//! ```
//! use belay_metrics::Registry;
//!
//! # fn main() -> belay_metrics::Result<()> {
//! let registry = Registry::new();
//! let outcomes = registry.counter_vec("jobs_finished_total", "Finished jobs.", &["queue", "outcome"])?;
//!
//! // Once, at setup:
//! let emails_ok = outcomes.with_label_values(&["emails", "succeeded"]);
//! // On the hot path: a single atomic add.
//! emails_ok.inc();
//!
//! // Pairs may come in any order.
//! let same = outcomes.with(&[("outcome", "succeeded"), ("queue", "emails")]);
//! assert_eq!(same.get(), 1);
//! # Ok(())
//! # }
//! ```

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, RwLock};

use crate::counter::Counter;
use crate::desc::Desc;
use crate::error::{Error, Result};
use crate::gauge::Gauge;
use crate::histogram::Histogram;
use crate::snapshot::{FamilySnapshot, MetricSnapshot, MetricValue};
use crate::summary::{Summary, SummaryOpts};
use crate::sync::{read, write};
use crate::time::SharedTimeSource;

/// A metric type that can be a child of a vec.
pub(crate) trait Child: Clone + Send + Sync + 'static {
    /// Per-family settings every child is built from.
    type Config: Clone + PartialEq + fmt::Debug + Send + Sync;

    fn create(config: &Self::Config, time: &SharedTimeSource) -> Self;

    fn value(&self) -> MetricValue;
}

impl Child for Counter {
    type Config = ();

    fn create(_: &(), _: &SharedTimeSource) -> Self {
        Counter::new()
    }

    fn value(&self) -> MetricValue {
        MetricValue::Counter(self.get())
    }
}

impl Child for Gauge {
    type Config = ();

    fn create(_: &(), _: &SharedTimeSource) -> Self {
        Gauge::new()
    }

    fn value(&self) -> MetricValue {
        MetricValue::Gauge(self.get())
    }
}

impl Child for Histogram {
    type Config = Arc<[f64]>;

    fn create(bounds: &Arc<[f64]>, time: &SharedTimeSource) -> Self {
        Histogram::from_bounds(Arc::clone(bounds), Arc::clone(time))
    }

    fn value(&self) -> MetricValue {
        MetricValue::Histogram(self.snapshot())
    }
}

impl Child for Summary {
    type Config = SummaryOpts;

    fn create(opts: &SummaryOpts, time: &SharedTimeSource) -> Self {
        Summary::from_validated(opts, Arc::clone(time))
    }

    fn value(&self) -> MetricValue {
        MetricValue::Summary(self.snapshot())
    }
}
