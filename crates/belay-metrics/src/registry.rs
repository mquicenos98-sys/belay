//! The registry: named metric families plus custom collectors, snapshotted
//! and rendered together.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use crate::counter::Counter;
use crate::desc::{Desc, MetricType};
use crate::error::{Error, Result};
use crate::gauge::Gauge;
use crate::histogram::{validate_buckets, Histogram};
use crate::snapshot::{FamilySnapshot, MetricValue};
use crate::summary::{Summary, SummaryOpts};
use crate::sync::{read, write};
use crate::text;
use crate::time::{self, SharedTimeSource};
use crate::vec::{Core, CounterVec, GaugeVec, HistogramVec, SummaryVec};

/// A source of metric families computed at scrape time, for values that live
/// elsewhere (store statistics, process memory) and would otherwise need a
/// background thread copying them into gauges.
///
/// `describe` is called once, at registration, to reserve the family names;
/// `collect` is called on every snapshot, outside the registry's lock, and
/// may return any subset of the described families (a value that cannot be
/// read right now is simply left out). Families it returns that were not
/// described, or whose type differs from the description, are dropped so a
/// misbehaving collector cannot produce an invalid exposition.
pub trait Collector: Send + Sync {
    /// The families this collector may produce.
    fn describe(&self) -> Vec<Desc>;

    /// Current values.
    fn collect(&self) -> Vec<FamilySnapshot>;
}

/// A set of metric families, rendered as one Prometheus scrape.
///
/// Cloning is cheap and clones share state, so one registry can be handed to
/// every component that records metrics and to the HTTP handler that serves
/// them.
///
/// # Registration
///
/// Each constructor validates the name and labels, then either creates the
/// family or, if a family of that name exists with an identical definition
/// (type, help text, label names, and summary options), returns the
/// existing one. Registration is therefore idempotent: two
/// components may both declare `belay_jobs_enqueued_total` and will share
/// its series. A same-named family with any difference in definition is an
/// [`Error::Conflict`]; a family whose exposed series names overlap another
/// family's (a counter `rpc_count` next to a histogram `rpc`) is an
/// [`Error::Collision`].
///
/// ```
/// use belay_metrics::{Error, Registry};
///
/// # fn main() -> belay_metrics::Result<()> {
/// let registry = Registry::new();
/// let a = registry.gauge("queue_depth", "Ready jobs.")?;
/// let b = registry.gauge("queue_depth", "Ready jobs.")?;
/// a.set(5.0);
/// assert_eq!(b.get(), 5.0);
///
/// let err = registry.counter("queue_depth", "Ready jobs.").unwrap_err();
/// assert!(matches!(err, Error::Conflict { .. }));
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Inner>,
}

struct Inner {
    state: RwLock<State>,
    time: SharedTimeSource,
}

#[derive(Default)]
struct State {
    families: BTreeMap<String, Family>,
    collectors: Vec<Registered>,
}

struct Registered {
    collector: Arc<dyn Collector>,
    descs: Vec<Desc>,
}

#[derive(Clone)]
enum Family {
    Counter(CounterVec),
    Gauge(GaugeVec),
    Histogram(HistogramVec),
    Summary(SummaryVec),
}

impl Family {
    fn desc(&self) -> &Desc {
        match self {
            Family::Counter(v) => v.desc(),
            Family::Gauge(v) => v.desc(),
            Family::Histogram(v) => v.desc(),
            Family::Summary(v) => v.desc(),
        }
    }

    fn snapshot(&self) -> FamilySnapshot {
        match self {
            Family::Counter(v) => v.snapshot(),
            Family::Gauge(v) => v.snapshot(),
            Family::Histogram(v) => v.snapshot(),
            Family::Summary(v) => v.snapshot(),
        }
    }
}

/// Checks `desc` against already-claimed descriptors; `bool` marks those
/// owned by a collector.
fn check_available<'a>(desc: &Desc, owners: impl Iterator<Item = (&'a Desc, bool)>) -> Result<()> {
    let claimed = desc.claimed_names();
    for (owner, by_collector) in owners {
        if owner.name() == desc.name() {
            let detail = if by_collector {
                "is already provided by a registered collector"
            } else {
                "is already registered"
            };
            return Err(Error::Conflict {
                name: desc.name().to_owned(),
                detail: detail.to_owned(),
            });
        }
        if let Some(series) = owner
            .claimed_names()
            .into_iter()
            .find(|s| claimed.contains(s))
        {
            return Err(Error::Collision {
                name: desc.name().to_owned(),
                series,
                owner: owner.name().to_owned(),
            });
        }
    }
    Ok(())
}
