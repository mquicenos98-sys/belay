//! Metrics about the running process, under the conventional `process_*`
//! names.
//!
//! Start time and uptime are available everywhere. Memory, thread and file
//! descriptor figures come from procfs (`/proc/self` on Linux); where it is
//! missing or unreadable those families are simply absent from the scrape.
//!
//! The start time is when the collector was created rather than when the OS
//! started the process: reading the real value needs the kernel clock tick
//! rate (`sysconf(_SC_CLK_TCK)`), which std does not expose. belay creates
//! the collector during startup, so the difference is milliseconds.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::desc::{Desc, MetricType};
use crate::registry::Collector;
use crate::snapshot::{FamilySnapshot, MetricValue};
use crate::time::{self, SharedTimeSource};

/// A [`Collector`] reporting process start time, uptime and, where procfs is
/// available, resident and virtual memory, thread count and open/maximum
/// file descriptors.
///
/// ```
/// use belay_metrics::{ProcessCollector, Registry};
/// use std::sync::Arc;
///
/// # fn main() -> belay_metrics::Result<()> {
/// let registry = Registry::new();
/// registry.register_collector(Arc::new(ProcessCollector::new()))?;
/// assert!(registry.render_prometheus().contains("# TYPE process_uptime_seconds gauge\n"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct ProcessCollector {
    proc_dir: Option<PathBuf>,
    start_time: SystemTime,
    time: SharedTimeSource,
    started: Duration,
    descs: Descs,
}

#[derive(Debug)]
struct Descs {
    start_time: Desc,
    uptime: Desc,
    resident: Desc,
    virtual_memory: Desc,
    threads: Desc,
    open_fds: Desc,
    max_fds: Desc,
}

fn gauge_desc(name: &str, help: &str) -> Desc {
    Desc::new(name, help, MetricType::Gauge, &[]).expect("process metric names are valid literals")
}
