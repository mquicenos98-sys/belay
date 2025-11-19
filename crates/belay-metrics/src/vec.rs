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

type Entry<M> = (Box<[String]>, M);

/// The shared state behind every vec type.
pub(crate) struct Core<M: Child> {
    desc: Desc,
    config: M::Config,
    time: SharedTimeSource,
    /// Children bucketed by the hash of their label values. Buckets almost
    /// always hold one entry; the `Vec` only matters on a hash collision.
    children: RwLock<HashMap<u64, Vec<Entry<M>>>>,
}

fn hash_values<S: AsRef<str>>(values: &[S]) -> u64 {
    // `str`'s Hash impl appends a terminator byte, so ["a", "bc"] and
    // ["ab", "c"] hash differently. DefaultHasher::new() has fixed keys, but
    // label values may come from API input, so why that is safe matters: the
    // outer map re-hashes this u64 with a random key, and lengthening one
    // bucket takes full 64-bit SipHash collisions, which are impractical to
    // mass-produce. Colliding tuples are still told apart by value.
    let mut hasher = DefaultHasher::new();
    for v in values {
        v.as_ref().hash(&mut hasher);
    }
    hasher.finish()
}

fn same_values(stored: &[String], wanted: &[&str]) -> bool {
    stored.len() == wanted.len() && stored.iter().zip(wanted).all(|(a, b)| a == b)
}

impl<M: Child> Core<M> {
    pub(crate) fn new(desc: Desc, config: M::Config, time: SharedTimeSource) -> Self {
        Core {
            desc,
            config,
            time,
            children: RwLock::new(HashMap::new()),
        }
    }

    fn get(&self, values: &[&str]) -> Result<M> {
        let expected = self.desc.label_names().len();
        if values.len() != expected {
            return Err(Error::LabelCount {
                expected,
                got: values.len(),
            });
        }
        let hash = hash_values(values);
        if let Some(bucket) = read(&self.children).get(&hash) {
            if let Some((_, child)) = bucket.iter().find(|(v, _)| same_values(v, values)) {
                return Ok(child.clone());
            }
        }
        let mut children = write(&self.children);
        let bucket = children.entry(hash).or_default();
        // Another thread may have created it between the two locks.
        if let Some((_, child)) = bucket.iter().find(|(v, _)| same_values(v, values)) {
            return Ok(child.clone());
        }
        let child = M::create(&self.config, &self.time);
        let owned = values.iter().map(|v| (*v).to_owned()).collect();
        bucket.push((owned, child.clone()));
        Ok(child)
    }

    fn get_by_pairs(&self, labels: &[(&str, &str)]) -> Result<M> {
        let names = self.desc.label_names();
        for (i, (name, _)) in labels.iter().enumerate() {
            if !names.iter().any(|n| n == name) {
                return Err(Error::UnknownLabel((*name).to_owned()));
            }
            if labels[..i].iter().any(|(earlier, _)| earlier == name) {
                return Err(Error::DuplicateLabel((*name).to_owned()));
            }
        }
        let mut values = Vec::with_capacity(names.len());
        for name in names {
            match labels.iter().find(|(n, _)| n == name) {
                Some(&(_, value)) => values.push(value),
                None => return Err(Error::MissingLabel(name.clone())),
            }
        }
        self.get(&values)
    }

    fn remove(&self, values: &[&str]) -> bool {
        let hash = hash_values(values);
        let mut children = write(&self.children);
        let Some(bucket) = children.get_mut(&hash) else {
            return false;
        };
        let before = bucket.len();
        bucket.retain(|(v, _)| !same_values(v, values));
        let removed = bucket.len() != before;
        if bucket.is_empty() {
            children.remove(&hash);
        }
        removed
    }

    fn len(&self) -> usize {
        read(&self.children).values().map(Vec::len).sum()
    }

    fn snapshot(&self) -> FamilySnapshot {
        // Clone the handles out so values (a summary sorts its samples) are
        // read without holding the lock that child creation needs.
        let mut children: Vec<Entry<M>> = read(&self.children)
            .values()
            .flatten()
            .map(|(values, child)| (values.clone(), child.clone()))
            .collect();
        children.sort_by(|a, b| a.0.cmp(&b.0));
        let mut family = FamilySnapshot::new(&self.desc);
        family.metrics = children
            .into_iter()
            .map(|(values, child)| MetricSnapshot {
                labels: self
                    .desc
                    .label_names()
                    .iter()
                    .cloned()
                    .zip(values.into_vec())
                    .collect(),
                value: child.value(),
            })
            .collect();
        family
    }
}

macro_rules! metric_vec {
    ($(#[$attr:meta])* $vec:ident => $metric:ident) => {
        $(#[$attr])*
        #[derive(Clone)]
        pub struct $vec {
            core: Arc<Core<$metric>>,
        }

        impl $vec {
            pub(crate) fn from_core(core: Core<$metric>) -> Self {
                $vec { core: Arc::new(core) }
            }

            pub(crate) fn snapshot(&self) -> FamilySnapshot {
                self.core.snapshot()
            }

            /// The family's name, help text, type and label names.
            pub fn desc(&self) -> &Desc {
                &self.core.desc
            }

            #[doc = concat!("The [`", stringify!($metric), "`] for `values`, given in label-name")]
            /// order, created on first use. Every call with equal values
            /// returns a handle to the same series.
            ///
            /// # Panics
            ///
            /// Panics if the number of values differs from the number of
            /// label names: a programming error, like indexing out of
            /// bounds. Use [`try_with_label_values`](Self::try_with_label_values)
            /// when the values come from input.
            pub fn with_label_values(&self, values: &[&str]) -> $metric {
                self.core
                    .get(values)
                    .unwrap_or_else(|e| panic!("metric {:?}: {e}", self.core.desc.name()))
            }

            /// Like [`with_label_values`](Self::with_label_values), but a
            /// wrong number of values is an [`Error::LabelCount`].
            pub fn try_with_label_values(&self, values: &[&str]) -> Result<$metric> {
                self.core.get(values)
            }

            /// The child for `(label, value)` pairs given in any order.
            ///
            /// # Panics
            ///
            /// Panics if a label is unknown, missing or repeated; see
            /// [`try_with`](Self::try_with).
            pub fn with(&self, labels: &[(&str, &str)]) -> $metric {
                self.core
                    .get_by_pairs(labels)
                    .unwrap_or_else(|e| panic!("metric {:?}: {e}", self.core.desc.name()))
            }

            /// Like [`with`](Self::with), returning an error for unknown,
            /// missing or repeated labels.
            pub fn try_with(&self, labels: &[(&str, &str)]) -> Result<$metric> {
                self.core.get_by_pairs(labels)
            }

            /// Removes the child for `values`, so it is no longer exported
            /// (e.g. after a queue is deleted). Returns whether it existed.
            /// Handles already held keep working but are detached: a later
            /// lookup of the same values starts a fresh series.
            pub fn remove_label_values(&self, values: &[&str]) -> bool {
                self.core.remove(values)
            }

            /// Removes every child.
            pub fn clear(&self) {
                write(&self.core.children).clear();
            }

            /// Number of children.
            pub fn len(&self) -> usize {
                self.core.len()
            }

            /// Whether the family has no children yet.
            pub fn is_empty(&self) -> bool {
                self.len() == 0
            }
        }

        impl fmt::Debug for $vec {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($vec))
                    .field("name", &self.core.desc.name())
                    .field("label_names", &self.core.desc.label_names())
                    .field("children", &self.len())
                    .finish()
            }
        }
    };
}

metric_vec! {
    /// A family of [`Counter`]s partitioned by labels.
    CounterVec => Counter
}

metric_vec! {
    /// A family of [`Gauge`]s partitioned by labels.
    GaugeVec => Gauge
}

metric_vec! {
    /// A family of [`Histogram`]s partitioned by labels, all sharing one
    /// bucket layout.
    HistogramVec => Histogram
}

metric_vec! {
    /// A family of [`Summary`]s partitioned by labels, all sharing one set of
    /// options.
    SummaryVec => Summary
}

impl HistogramVec {
    /// The finite bucket bounds shared by every child.
    pub fn buckets(&self) -> &[f64] {
        &self.core.config
    }
}

impl SummaryVec {
    /// The (normalised) options shared by every child.
    pub fn opts(&self) -> &SummaryOpts {
        &self.core.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::MetricType;
    use crate::time::monotonic;

    fn counters(labels: &[&str]) -> CounterVec {
        let desc = Desc::new("jobs_total", "Jobs.", MetricType::Counter, labels).unwrap();
        CounterVec::from_core(Core::new(desc, (), monotonic()))
    }

    #[test]
    fn equal_values_share_one_series() {
        let vec = counters(&["queue"]);
        vec.with_label_values(&["emails"]).inc();
        vec.with_label_values(&["emails"]).inc_by(2);
        vec.with_label_values(&["reports"]).inc();
        assert_eq!(vec.with_label_values(&["emails"]).get(), 3);
        assert_eq!(vec.len(), 2);
    }

    #[test]
    fn values_are_not_split_ambiguously() {
        let vec = counters(&["a", "b"]);
        vec.with_label_values(&["x", "yz"]).inc();
        assert_eq!(vec.with_label_values(&["xy", "z"]).get(), 0);
        assert_eq!(vec.len(), 2);
    }

    #[test]
    fn wrong_arity_is_an_error() {
        let vec = counters(&["queue", "state"]);
        assert_eq!(
            vec.try_with_label_values(&["emails"]).unwrap_err(),
            Error::LabelCount {
                expected: 2,
                got: 1
            }
        );
        assert!(vec.is_empty());
    }

    #[test]
    #[should_panic(expected = "metric \"jobs_total\": expected 1 label value, got 2")]
    fn with_label_values_panics_on_wrong_arity() {
        counters(&["queue"]).with_label_values(&["a", "b"]);
    }

    #[test]
    fn pairs_are_matched_by_name() {
        let vec = counters(&["queue", "state"]);
        vec.with(&[("state", "ready"), ("queue", "emails")]).inc();
        assert_eq!(vec.with_label_values(&["emails", "ready"]).get(), 1);

        assert_eq!(
            vec.try_with(&[("queue", "a"), ("stat", "b")]).unwrap_err(),
            Error::UnknownLabel("stat".into())
        );
        assert_eq!(
            vec.try_with(&[("queue", "a")]).unwrap_err(),
            Error::MissingLabel("state".into())
        );
        assert_eq!(
            vec.try_with(&[("queue", "a"), ("queue", "b"), ("state", "c")])
                .unwrap_err(),
            Error::DuplicateLabel("queue".into())
        );
    }

    #[test]
    fn a_family_without_labels_has_one_child() {
        let vec = counters(&[]);
        vec.with_label_values(&[]).inc();
        vec.with(&[]).inc();
        assert_eq!(vec.len(), 1);
        assert_eq!(vec.snapshot().metrics[0].value.as_counter(), Some(2));
    }

    #[test]
    fn removed_children_detach_existing_handles() {
        let vec = counters(&["queue"]);
        let old = vec.with_label_values(&["emails"]);
        old.inc();
        assert!(vec.remove_label_values(&["emails"]));
        assert!(!vec.remove_label_values(&["emails"]));
        old.inc();
        assert_eq!(old.get(), 2);
        assert_eq!(vec.with_label_values(&["emails"]).get(), 0);

        vec.with_label_values(&["other"]);
        vec.clear();
        assert!(vec.is_empty());
    }

    #[test]
    fn snapshot_sorts_children_and_keeps_label_order() {
        let vec = counters(&["queue", "state"]);
        vec.with_label_values(&["b", "ready"]).inc();
        vec.with_label_values(&["a", "running"]).inc_by(2);
        vec.with_label_values(&["a", "dead"]).inc_by(3);
        let snap = vec.snapshot();
        let rows: Vec<(Vec<(String, String)>, u64)> = snap
            .metrics
            .iter()
            .map(|m| (m.labels.clone(), m.value.as_counter().unwrap()))
            .collect();
        let pair = |q: &str, s: &str| vec![("queue".into(), q.into()), ("state".into(), s.into())];
        assert_eq!(
            rows,
            [
                (pair("a", "dead"), 3),
                (pair("a", "running"), 2),
                (pair("b", "ready"), 1)
            ]
        );
    }

    #[test]
    fn empty_and_unicode_label_values_are_allowed() {
        let vec = counters(&["queue"]);
        vec.with_label_values(&[""]).inc();
        vec.with_label_values(&["colas-ñandú"]).inc();
        assert_eq!(vec.len(), 2);
    }

    #[test]
    fn debug_shows_shape_not_values() {
        let vec = counters(&["queue"]);
        vec.with_label_values(&["a"]);
        assert_eq!(
            format!("{vec:?}"),
            r#"CounterVec { name: "jobs_total", label_names: ["queue"], children: 1 }"#
        );
    }
}
