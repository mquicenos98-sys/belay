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

impl Descs {
    fn new() -> Self {
        Descs {
            start_time: gauge_desc(
                "process_start_time_seconds",
                "Start time of the process since the Unix epoch in seconds.",
            ),
            uptime: gauge_desc(
                "process_uptime_seconds",
                "Seconds since the process started.",
            ),
            resident: gauge_desc(
                "process_resident_memory_bytes",
                "Resident memory size in bytes.",
            ),
            virtual_memory: gauge_desc(
                "process_virtual_memory_bytes",
                "Virtual memory size in bytes.",
            ),
            threads: gauge_desc("process_threads", "Number of OS threads in the process."),
            open_fds: gauge_desc("process_open_fds", "Number of open file descriptors."),
            max_fds: gauge_desc(
                "process_max_fds",
                "Maximum number of open file descriptors.",
            ),
        }
    }

    fn all(&self) -> Vec<Desc> {
        vec![
            self.start_time.clone(),
            self.uptime.clone(),
            self.resident.clone(),
            self.virtual_memory.clone(),
            self.threads.clone(),
            self.open_fds.clone(),
            self.max_fds.clone(),
        ]
    }
}

impl ProcessCollector {
    /// A collector measuring uptime with the process-wide monotonic clock and
    /// reading `/proc/self` on Linux.
    pub fn new() -> Self {
        Self::with_time_source(time::monotonic())
    }

    /// A collector measuring uptime with `time`.
    pub fn with_time_source(time: SharedTimeSource) -> Self {
        let proc_dir = if cfg!(target_os = "linux") {
            Some(PathBuf::from("/proc/self"))
        } else {
            None
        };
        ProcessCollector {
            proc_dir,
            start_time: SystemTime::now(),
            started: time.now(),
            time,
            descs: Descs::new(),
        }
    }

    /// Reads procfs files (`status`, `limits`, `fd/`) from `dir` instead of
    /// the platform default; `None` disables them. Tests point this at a
    /// fixture directory.
    #[must_use]
    pub fn proc_dir(mut self, dir: Option<&Path>) -> Self {
        self.proc_dir = dir.map(Path::to_path_buf);
        self
    }

    /// Overrides the reported start time.
    #[must_use]
    pub fn start_time(mut self, at: SystemTime) -> Self {
        self.start_time = at;
        self
    }

    fn gauge(desc: &Desc, value: f64) -> FamilySnapshot {
        FamilySnapshot::new(desc).with_metric(&[], MetricValue::Gauge(value))
    }
}

impl Default for ProcessCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for ProcessCollector {
    fn describe(&self) -> Vec<Desc> {
        self.descs.all()
    }

    fn collect(&self) -> Vec<FamilySnapshot> {
        let d = &self.descs;
        let since_epoch = self
            .start_time
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let uptime = self.time.now().saturating_sub(self.started);
        let mut out = vec![
            Self::gauge(&d.start_time, since_epoch.as_secs_f64()),
            Self::gauge(&d.uptime, uptime.as_secs_f64()),
        ];
        let Some(dir) = &self.proc_dir else {
            return out;
        };
        if let Ok(text) = fs::read_to_string(dir.join("status")) {
            let status = parse_status(&text);
            let figures = [
                (&d.resident, status.resident_bytes),
                (&d.virtual_memory, status.virtual_bytes),
                (&d.threads, status.threads),
            ];
            for (desc, value) in figures {
                if let Some(v) = value {
                    out.push(Self::gauge(desc, v as f64));
                }
            }
        }
        if let Ok(entries) = fs::read_dir(dir.join("fd")) {
            // The directory handle used for this listing shows up in it, so
            // the figure can be one higher than at rest; the reference Go
            // client has the same quirk.
            out.push(Self::gauge(&d.open_fds, entries.count() as f64));
        }
        if let Some(max) = fs::read_to_string(dir.join("limits"))
            .ok()
            .and_then(|text| parse_max_open_files(&text))
        {
            out.push(Self::gauge(&d.max_fds, max as f64));
        }
        out
    }
}

/// The fields of `/proc/<pid>/status` the collector reports.
#[derive(Debug, Default, PartialEq, Eq)]
struct Status {
    resident_bytes: Option<u64>,
    virtual_bytes: Option<u64>,
    threads: Option<u64>,
}

/// Parses `/proc/<pid>/status` (`Key:\tvalue [kB]` lines, see proc(5)).
fn parse_status(text: &str) -> Status {
    let mut status = Status::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key {
            "VmRSS" => status.resident_bytes = parse_kib(value),
            "VmSize" => status.virtual_bytes = parse_kib(value),
            "Threads" => status.threads = value.trim().parse().ok(),
            _ => {}
        }
    }
    status
}

/// Parses `  1234 kB` into bytes; the kernel always reports KiB there.
fn parse_kib(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let n: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("kB") => n.checked_mul(1024),
        _ => None,
    }
}

/// Extracts the soft limit from the `Max open files` row of
/// `/proc/<pid>/limits`; `unlimited` yields `None`.
fn parse_max_open_files(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("Max open files"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|soft| soft.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "Name:\tbelay\nUmask:\t0022\nState:\tS (sleeping)\n\
        VmPeak:\t  230000 kB\nVmSize:\t  220000 kB\nVmRSS:\t   15360 kB\n\
        Threads:\t12\nSigQ:\t0/63432\n";

    const LIMITS: &str =
        "Limit                     Soft Limit           Hard Limit           Units     \n\
        Max cpu time              unlimited            unlimited            seconds   \n\
        Max open files            1024                 524288               files     \n\
        Max locked memory         8388608              8388608              bytes     \n";

    #[test]
    fn parses_memory_and_threads_from_status() {
        assert_eq!(
            parse_status(STATUS),
            Status {
                resident_bytes: Some(15360 * 1024),
                virtual_bytes: Some(220000 * 1024),
                threads: Some(12),
            }
        );
    }

    #[test]
    fn missing_or_malformed_status_fields_are_none() {
        let status = parse_status("VmRSS:\tlots kB\nVmSize:\t12 MB\nThreads:\n");
        assert_eq!(status, Status::default());
    }

    #[test]
    fn parses_the_soft_open_files_limit() {
        assert_eq!(parse_max_open_files(LIMITS), Some(1024));
        let unlimited = LIMITS.replace(
            "1024                 524288",
            "unlimited            unlimited",
        );
        assert_eq!(parse_max_open_files(&unlimited), None);
        assert_eq!(parse_max_open_files(""), None);
    }

    #[test]
    fn without_procfs_only_time_families_are_collected() {
        let collector = ProcessCollector::new().proc_dir(None);
        let names: Vec<String> = collector.collect().into_iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            ["process_start_time_seconds", "process_uptime_seconds"]
        );
        assert_eq!(collector.describe().len(), 7);
    }

    #[test]
    fn unreadable_proc_dir_is_not_an_error() {
        let missing = std::env::temp_dir().join("belay-metrics-no-such-proc-dir");
        let collector = ProcessCollector::new().proc_dir(Some(&missing));
        assert_eq!(collector.collect().len(), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_the_real_procfs_on_linux() {
        let families = ProcessCollector::new().collect();
        let rss = families
            .iter()
            .find(|f| f.name == "process_resident_memory_bytes")
            .and_then(|f| f.metrics[0].value.as_gauge());
        assert!(rss.is_some_and(|v| v > 0.0));
    }
}
