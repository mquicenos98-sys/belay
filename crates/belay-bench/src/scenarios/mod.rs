//! The benchmark scenarios. Each is a function that runs its cases
//! (skipping those the filters exclude) and records a result per case.
//!
//! | scenario | measures |
//! |---|---|
//! | `store-write` | [`LogKv`](belay_store::LogKv) writes on a real directory by sync policy, batch size, value size and writer threads |
//! | `store-recover` | opening a store: replaying the log versus loading a snapshot, by data size |
//! | `queue-embedded` | [`QueueEngine`](belay_queue::QueueEngine) enqueue, lease and complete on `MemKv` and `LogKv`, by thread count |
//! | `queue-http` | the same operations through `belay-server` and `belay-client` over loopback, by client count |
//! | `worker-e2e` | the worker runtime draining a backlog, and how fast an idle worker picks up a new job |
//! | `cron` | `CronExpr::next_after` for typical expressions, in UTC and in zones with daylight saving time |
//! | `json` | serializing, parsing and decoding job documents of three sizes |

use crate::session::Session;
use crate::Result;

mod cron;
mod json;
mod queue;
mod store;
mod worker;

/// A named benchmark.
#[derive(Debug)]
pub struct Scenario {
    /// Name used on the command line and in results.
    pub name: &'static str,
    /// One-line description.
    pub summary: &'static str,
    /// Runs the scenario's cases.
    pub run: fn(&mut Session<'_>) -> Result<()>,
}

/// Every scenario, in the order `all` runs them.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: store::WRITE,
        summary: "LogKv writes by sync policy, batch size, value size and writers",
        run: store::run_write,
    },
    Scenario {
        name: store::RECOVER,
        summary: "LogKv open time: log replay versus snapshot load, by size",
        run: store::run_recover,
    },
    Scenario {
        name: queue::EMBEDDED,
        summary: "QueueEngine enqueue/lease/complete on MemKv and LogKv, by threads",
        run: queue::run_embedded,
    },
    Scenario {
        name: queue::HTTP,
        summary: "the same operations through belay-server and belay-client, by clients",
        run: queue::run_http,
    },
    Scenario {
        name: worker::NAME,
        summary: "worker runtime: backlog drain rate and idle pickup latency",
        run: worker::run,
    },
    Scenario {
        name: cron::NAME,
        summary: "CronExpr::next_after in UTC and daylight-saving zones",
        run: cron::run,
    },
    Scenario {
        name: json::NAME,
        summary: "serialize, parse and decode job documents of three sizes",
        run: json::run,
    },
];

/// The scenario called `name`.
#[must_use]
pub fn find(name: &str) -> Option<&'static Scenario> {
    SCENARIOS.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_findable() {
        for (i, s) in SCENARIOS.iter().enumerate() {
            assert!(SCENARIOS[..i].iter().all(|other| other.name != s.name));
            assert_eq!(find(s.name).map(|f| f.name), Some(s.name));
        }
        assert!(find("store").is_none());
    }
}
