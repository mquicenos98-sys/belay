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
/// (type, help text, label names, and bucket layout or summary options),
/// returns the existing one. Registration is therefore idempotent: two
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

impl State {
    fn owners(&self) -> impl Iterator<Item = (&Desc, bool)> {
        self.families.values().map(|f| (f.desc(), false)).chain(
            self.collectors
                .iter()
                .flat_map(|c| c.descs.iter().map(|d| (d, true))),
        )
    }
}

impl Registry {
    /// An empty registry whose timers and summaries use the process-wide
    /// monotonic clock.
    pub fn new() -> Self {
        Self::with_time_source(time::monotonic())
    }

    /// An empty registry whose timers and summary windows read `time`.
    pub fn with_time_source(time: SharedTimeSource) -> Self {
        Registry {
            inner: Arc::new(Inner {
                state: RwLock::new(State::default()),
                time,
            }),
        }
    }

    /// The clock handed to every histogram and summary created here.
    pub fn time_source(&self) -> &SharedTimeSource {
        &self.inner.time
    }

    /// Registers (or returns) an unlabelled counter.
    pub fn counter(&self, name: &str, help: &str) -> Result<Counter> {
        self.counter_vec(name, help, &[])?
            .try_with_label_values(&[])
    }

    /// Registers (or returns) a labelled counter family.
    pub fn counter_vec(&self, name: &str, help: &str, label_names: &[&str]) -> Result<CounterVec> {
        let desc = Desc::new(name, help, MetricType::Counter, label_names)?;
        self.register(
            desc,
            |family| match family {
                Family::Counter(v) => Some(v.clone()),
                _ => None,
            },
            |desc, time| {
                let v = CounterVec::from_core(Core::new(desc, (), time));
                (Family::Counter(v.clone()), v)
            },
        )
    }

    /// Registers (or returns) an unlabelled gauge.
    pub fn gauge(&self, name: &str, help: &str) -> Result<Gauge> {
        self.gauge_vec(name, help, &[])?.try_with_label_values(&[])
    }

    /// Registers (or returns) a labelled gauge family.
    pub fn gauge_vec(&self, name: &str, help: &str, label_names: &[&str]) -> Result<GaugeVec> {
        let desc = Desc::new(name, help, MetricType::Gauge, label_names)?;
        self.register(
            desc,
            |family| match family {
                Family::Gauge(v) => Some(v.clone()),
                _ => None,
            },
            |desc, time| {
                let v = GaugeVec::from_core(Core::new(desc, (), time));
                (Family::Gauge(v.clone()), v)
            },
        )
    }

    /// Registers (or returns) an unlabelled histogram with the given finite
    /// bucket upper bounds (see [`DEFAULT_BUCKETS`](crate::DEFAULT_BUCKETS)).
    pub fn histogram(&self, name: &str, help: &str, buckets: &[f64]) -> Result<Histogram> {
        self.histogram_vec(name, help, &[], buckets)?
            .try_with_label_values(&[])
    }

    /// Registers (or returns) a labelled histogram family.
    pub fn histogram_vec(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
        buckets: &[f64],
    ) -> Result<HistogramVec> {
        let desc = Desc::new(name, help, MetricType::Histogram, label_names)?;
        let bounds = validate_buckets(buckets)?;
        self.register(
            desc,
            |family| match family {
                Family::Histogram(v) if v.buckets() == &bounds[..] => Some(v.clone()),
                _ => None,
            },
            |desc, time| {
                let v = HistogramVec::from_core(Core::new(desc, Arc::clone(&bounds), time));
                (Family::Histogram(v.clone()), v)
            },
        )
    }

    /// Registers (or returns) an unlabelled summary.
    pub fn summary(&self, name: &str, help: &str, opts: SummaryOpts) -> Result<Summary> {
        self.summary_vec(name, help, &[], opts)?
            .try_with_label_values(&[])
    }

    /// Registers (or returns) a labelled summary family.
    pub fn summary_vec(
        &self,
        name: &str,
        help: &str,
        label_names: &[&str],
        opts: SummaryOpts,
    ) -> Result<SummaryVec> {
        let desc = Desc::new(name, help, MetricType::Summary, label_names)?;
        let opts = opts.validated()?;
        self.register(
            desc,
            |family| match family {
                Family::Summary(v) if v.opts() == &opts => Some(v.clone()),
                _ => None,
            },
            |desc, time| {
                let v = SummaryVec::from_core(Core::new(desc, opts.clone(), time));
                (Family::Summary(v.clone()), v)
            },
        )
    }

    /// The shared registration path: return the existing family when the
    /// definition matches, reject any other use of the name or of its
    /// series names, and otherwise insert what `create` builds.
    fn register<T>(
        &self,
        desc: Desc,
        existing: impl FnOnce(&Family) -> Option<T>,
        create: impl FnOnce(Desc, SharedTimeSource) -> (Family, T),
    ) -> Result<T> {
        let mut state = write(&self.inner.state);
        if let Some(family) = state.families.get(desc.name()) {
            if family.desc() == &desc {
                if let Some(found) = existing(family) {
                    return Ok(found);
                }
            }
            return Err(Error::Conflict {
                name: desc.name().to_owned(),
                detail: family.desc().conflict_detail(&desc),
            });
        }
        check_available(&desc, state.owners())?;
        let (family, handle) = create(desc, Arc::clone(&self.inner.time));
        state
            .families
            .insert(family.desc().name().to_owned(), family);
        Ok(handle)
    }

    /// Registers a gauge whose value is computed by `read` at scrape time.
    ///
    /// ```
    /// use belay_metrics::Registry;
    /// use std::sync::atomic::{AtomicUsize, Ordering};
    /// use std::sync::Arc;
    ///
    /// # fn main() -> belay_metrics::Result<()> {
    /// let registry = Registry::new();
    /// let pending = Arc::new(AtomicUsize::new(3));
    /// let source = Arc::clone(&pending);
    /// registry.register_gauge_fn("store_pending_writes", "Writes awaiting fsync.", move || {
    ///     source.load(Ordering::Relaxed) as f64
    /// })?;
    /// assert!(registry.render_prometheus().contains("store_pending_writes 3\n"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn register_gauge_fn<F>(&self, name: &str, help: &str, read: F) -> Result<()>
    where
        F: Fn() -> f64 + Send + Sync + 'static,
    {
        let desc = Desc::new(name, help, MetricType::Gauge, &[])?;
        self.register_collector(Arc::new(FnMetric {
            desc,
            read: Box::new(move || MetricValue::Gauge(read())),
        }))
    }

    /// Registers a counter whose value is computed by `read` at scrape time,
    /// for totals another component already keeps. `read` must never
    /// decrease, or rate calculations downstream will see a reset.
    pub fn register_counter_fn<F>(&self, name: &str, help: &str, read: F) -> Result<()>
    where
        F: Fn() -> u64 + Send + Sync + 'static,
    {
        let desc = Desc::new(name, help, MetricType::Counter, &[])?;
        self.register_collector(Arc::new(FnMetric {
            desc,
            read: Box::new(move || MetricValue::Counter(read())),
        }))
    }

    /// Adds a collector. Its described families must not clash with any
    /// registered family, other collector, or each other.
    pub fn register_collector(&self, collector: Arc<dyn Collector>) -> Result<()> {
        // Describe outside the lock: collector code is arbitrary.
        let descs = collector.describe();
        let mut state = write(&self.inner.state);
        for (i, desc) in descs.iter().enumerate() {
            check_available(desc, state.owners())?;
            check_available(desc, descs[..i].iter().map(|d| (d, true)))?;
        }
        state.collectors.push(Registered { collector, descs });
        Ok(())
    }

    /// Removes a collector previously registered (compared by identity).
    /// Returns whether it was found.
    pub fn unregister_collector(&self, collector: &Arc<dyn Collector>) -> bool {
        let target = Arc::as_ptr(collector) as *const ();
        let mut state = write(&self.inner.state);
        let before = state.collectors.len();
        state
            .collectors
            .retain(|r| Arc::as_ptr(&r.collector) as *const () != target);
        state.collectors.len() != before
    }

    /// Removes the family called `name`. Handles already held keep working
    /// but are no longer exported, and registering the name again starts a
    /// fresh family. Returns whether the family existed.
    pub fn unregister(&self, name: &str) -> bool {
        write(&self.inner.state).families.remove(name).is_some()
    }

    /// Whether a family called `name` is registered (collectors excluded).
    pub fn contains(&self, name: &str) -> bool {
        read(&self.inner.state).families.contains_key(name)
    }

    /// Every family, from registered metrics and collectors, sorted by name.
    ///
    /// Families without series (a vec with no children yet) are included
    /// with an empty `metrics` list; the text format leaves them out.
    pub fn snapshot(&self) -> Vec<FamilySnapshot> {
        let (families, collectors) = {
            let state = read(&self.inner.state);
            let families: Vec<Family> = state.families.values().cloned().collect();
            let collectors: Vec<(Arc<dyn Collector>, Vec<Desc>)> = state
                .collectors
                .iter()
                .map(|r| (Arc::clone(&r.collector), r.descs.clone()))
                .collect();
            (families, collectors)
        };
        let mut out: Vec<FamilySnapshot> = families.iter().map(Family::snapshot).collect();
        for (collector, descs) in collectors {
            let first = out.len();
            for family in collector.collect() {
                let described = descs
                    .iter()
                    .any(|d| d.name() == family.name && d.kind() == family.kind);
                if !described {
                    continue;
                }
                // A collector may split one family across several snapshots;
                // the format needs them merged under one header.
                match out[first..].iter_mut().find(|f| f.name == family.name) {
                    Some(merged) => merged.metrics.extend(family.metrics),
                    None => out.push(family),
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Renders every family in the Prometheus text format, version 0.0.4.
    /// Serve it with [`text::CONTENT_TYPE`].
    pub fn render_prometheus(&self) -> String {
        text::encode(&self.snapshot())
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = read(&self.inner.state);
        f.debug_struct("Registry")
            .field("families", &state.families.keys().collect::<Vec<_>>())
            .field("collectors", &state.collectors.len())
            .finish()
    }
}

/// A single unlabelled series computed by a closure.
struct FnMetric {
    desc: Desc,
    read: Box<dyn Fn() -> MetricValue + Send + Sync>,
}

impl Collector for FnMetric {
    fn describe(&self) -> Vec<Desc> {
        vec![self.desc.clone()]
    }

    fn collect(&self) -> Vec<FamilySnapshot> {
        vec![FamilySnapshot::new(&self.desc).with_metric(&[], (self.read)())]
    }
}
