//! The worker registry: who has been leasing, when they were last heard
//! from, and what they hold.
//!
//! Nothing here is persisted. Workers announce themselves with every lease
//! and heartbeat, so the registry refills within one lease interval of a
//! restart; `active_jobs` is re-seeded from the running jobs' leases when
//! the engine opens, so the numbers are right immediately.

use std::collections::BTreeMap;
use std::time::Duration;

use belay_common::Timestamp;
use belay_proto::api::WorkerInfo;
use belay_proto::{QueueName, WorkerId};

/// A completed attempt, for the per-worker counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Leased,
    Succeeded,
    Failed,
}

#[derive(Debug)]
struct Entry {
    info: WorkerInfo,
    lost: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Workers {
    entries: BTreeMap<WorkerId, Entry>,
}

impl Workers {
    fn entry(&mut self, id: &WorkerId, now: Timestamp) -> &mut Entry {
        self.entries.entry(id.clone()).or_insert_with(|| Entry {
            info: WorkerInfo::new(id.clone(), now),
            lost: false,
        })
    }

    /// Records a request from `id` at `now`, and the queues it serves when
    /// it is a lease request. Returns whether the worker is new or back
    /// after being reported lost, i.e. whether to announce it.
    pub(crate) fn seen(
        &mut self,
        id: &WorkerId,
        queues: Option<&[QueueName]>,
        now: Timestamp,
    ) -> bool {
        let fresh = !self.entries.contains_key(id);
        let entry = self.entry(id, now);
        let announce = fresh || entry.lost;
        entry.lost = false;
        entry.info.last_seen = entry.info.last_seen.max(now);
        if let Some(queues) = queues {
            entry.info.queues = queues.to_vec();
        }
        announce
    }

    /// A job leased by `id` started running (or was found running when
    /// the engine opened).
    pub(crate) fn acquire(&mut self, id: &WorkerId, now: Timestamp) {
        let entry = self.entry(id, now);
        entry.info.active_jobs = entry.info.active_jobs.saturating_add(1);
    }

    /// A job leased by `id` stopped running, however that happened.
    pub(crate) fn release(&mut self, id: &WorkerId) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.info.active_jobs = entry.info.active_jobs.saturating_sub(1);
        }
    }

    pub(crate) fn record(&mut self, id: &WorkerId, outcome: Outcome, now: Timestamp) {
        let info = &mut self.entry(id, now).info;
        match outcome {
            Outcome::Leased => info.leased += 1,
            Outcome::Succeeded => info.succeeded += 1,
            Outcome::Failed => info.failed += 1,
        }
    }

    /// Marks workers silent for `timeout` as lost, returning them with
    /// their last-seen time, and forgets lost workers that hold nothing and
    /// have been silent for `timeout + retention`.
    pub(crate) fn sweep(
        &mut self,
        now: Timestamp,
        timeout: Duration,
        retention: Duration,
    ) -> Vec<(WorkerId, Timestamp)> {
        let mut lost = Vec::new();
        for (id, entry) in &mut self.entries {
            if !entry.lost && entry.info.is_lost(now, timeout) {
                entry.lost = true;
                lost.push((id.clone(), entry.info.last_seen));
            }
        }
        let forget_after = timeout.saturating_add(retention);
        self.entries.retain(|_, e| {
            !(e.lost && e.info.active_jobs == 0 && e.info.is_lost(now, forget_after))
        });
        lost
    }

    pub(crate) fn list(&self) -> Vec<WorkerInfo> {
        self.entries.values().map(|e| e.info.clone()).collect()
    }

    pub(crate) fn get(&self, id: &WorkerId) -> Option<&WorkerInfo> {
        self.entries.get(id).map(|e| &e.info)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_secs(10_000 + secs)
    }

    fn w(name: &'static str) -> WorkerId {
        WorkerId::from_static(name)
    }

    #[test]
    fn first_sight_and_return_from_lost_are_announced() {
        let mut ws = Workers::default();
        let q = [QueueName::from_static("emails")];
        assert!(ws.seen(&w("a"), Some(&q), at(0)));
        assert!(!ws.seen(&w("a"), None, at(5)));
        let info = ws.get(&w("a")).expect("registered");
        assert_eq!(info.queues, q);
        assert_eq!((info.first_seen, info.last_seen), (at(0), at(5)));

        let hour = Duration::from_secs(3600);
        let lost = ws.sweep(at(200), Duration::from_secs(60), hour);
        assert_eq!(lost, [(w("a"), at(5))]);
        assert!(ws.sweep(at(300), Duration::from_secs(60), hour).is_empty());
        assert!(ws.seen(&w("a"), None, at(301)), "back from lost");
    }

    #[test]
    fn counters_and_active_jobs() {
        let mut ws = Workers::default();
        ws.seen(&w("a"), None, at(0));
        ws.acquire(&w("a"), at(0));
        ws.acquire(&w("a"), at(0));
        ws.record(&w("a"), Outcome::Leased, at(0));
        ws.record(&w("a"), Outcome::Leased, at(0));
        ws.release(&w("a"));
        ws.record(&w("a"), Outcome::Succeeded, at(1));
        ws.release(&w("a"));
        ws.release(&w("a"));
        ws.record(&w("a"), Outcome::Failed, at(2));
        let info = ws.get(&w("a")).expect("registered");
        assert_eq!(
            (info.active_jobs, info.leased, info.succeeded, info.failed),
            (0, 2, 1, 1)
        );
        ws.release(&w("unknown"));
        assert_eq!(ws.len(), 1);
    }

    #[test]
    fn lost_workers_are_forgotten_only_when_idle() {
        let mut ws = Workers::default();
        ws.seen(&w("busy"), None, at(0));
        ws.acquire(&w("busy"), at(0));
        ws.seen(&w("idle"), None, at(0));
        let timeout = Duration::from_secs(60);
        let retention = Duration::from_secs(100);
        assert_eq!(ws.sweep(at(61), timeout, retention).len(), 2);
        ws.sweep(at(161), timeout, retention);
        let names: Vec<String> = ws.list().into_iter().map(|i| i.id.to_string()).collect();
        assert_eq!(names, ["busy"]);
    }
}
