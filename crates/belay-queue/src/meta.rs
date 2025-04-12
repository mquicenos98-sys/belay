//! The compact per-job entry the engine keeps in memory.
//!
//! The store already holds every job document (the log-structured store
//! keeps its whole table in memory), so a second decoded copy of each job
//! would double the footprint of payload-heavy queues. Instead the engine
//! keeps a [`Meta`] per job: exactly the fields its indexes are keyed on,
//! plus the lease token so stale worker calls are rejected without a store
//! read. Documents are decoded when a job changes, which every change
//! must do anyway to write the new version.

use belay_common::Timestamp;
use belay_proto::{Job, JobId, JobState, LeaseToken, QueueName, TenantId, WorkerId};

/// Index fields of one job, derived from its document by [`Meta::of`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) queue: QueueName,
    pub(crate) state: JobState,
    pub(crate) priority: i16,
    /// When the job became (or becomes) eligible to run.
    pub(crate) run_at: Timestamp,
    pub(crate) created_at: Timestamp,
    pub(crate) finished_at: Option<Timestamp>,
    pub(crate) tenant: Option<TenantId>,
    pub(crate) lease: Option<LeaseMeta>,
}

/// The parts of a running job's lease the engine checks and indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeaseMeta {
    pub(crate) worker: WorkerId,
    pub(crate) token: LeaseToken,
    pub(crate) expires_at: Timestamp,
}

impl Meta {
    pub(crate) fn of(job: &Job) -> Meta {
        Meta {
            queue: job.spec.queue.clone(),
            state: job.state,
            priority: job.spec.priority,
            run_at: job.run_at,
            created_at: job.created_at,
            finished_at: job.finished_at,
            tenant: job.spec.tenant.clone(),
            lease: job.lease.as_ref().map(|l| LeaseMeta {
                worker: l.worker.clone(),
                token: l.token,
                expires_at: l.expires_at,
            }),
        }
    }

    /// Position in the queue's ready order.
    pub(crate) fn ready_key(&self, id: JobId) -> ReadyKey {
        ReadyKey::new(self.priority, self.run_at, id)
    }

    /// When a running job's lease runs out. A running job without a lease
    /// is a damaged record; it counts as expired long ago so the reaper
    /// recovers it (the same rule as [`Job::lease_expired`]).
    pub(crate) fn lease_deadline(&self) -> Timestamp {
        self.lease
            .as_ref()
            .map_or(Timestamp::MIN, |lease| lease.expires_at)
    }

    /// When a terminal job finished, for retention. Falls back to its
    /// creation time for records that lack one.
    pub(crate) fn finished_or_created(&self) -> Timestamp {
        self.finished_at.unwrap_or(self.created_at)
    }

    /// Whether `token` is the running job's current lease token and the
    /// lease has not run out at `now`.
    pub(crate) fn holds_lease(&self, token: LeaseToken, now: Timestamp) -> bool {
        self.state == JobState::Running
            && self
                .lease
                .as_ref()
                .is_some_and(|l| l.token == token && now < l.expires_at)
    }
}

/// Order of ready jobs within one tenant: highest priority first, then the
/// job that has been eligible longest, then the oldest id.
///
/// `rank` is the negated priority so that the derived ascending order puts
/// the highest priority first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ReadyKey {
    rank: i32,
    run_at: Timestamp,
    pub(crate) id: JobId,
}

impl ReadyKey {
    pub(crate) fn new(priority: i16, run_at: Timestamp, id: JobId) -> ReadyKey {
        ReadyKey {
            rank: -i32::from(priority),
            run_at,
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_common::Ulid;
    use belay_proto::{JobSpec, Lease};
    use std::time::Duration;

    fn id(n: u128) -> JobId {
        JobId::from_ulid(Ulid::from_u128(n))
    }

    #[test]
    fn ready_order_is_priority_then_age_then_id() {
        let t = Timestamp::from_secs;
        let mut keys = vec![
            ReadyKey::new(0, t(5), id(1)),
            ReadyKey::new(10, t(9), id(2)),
            ReadyKey::new(0, t(1), id(3)),
            ReadyKey::new(-1000, t(0), id(4)),
            ReadyKey::new(0, t(1), id(0)),
            ReadyKey::new(i16::MIN, t(0), id(5)),
        ];
        keys.sort();
        let order: Vec<u128> = keys.iter().map(|k| k.id.ulid().to_u128()).collect();
        assert_eq!(order, [2, 0, 3, 1, 4, 5]);
    }

    #[test]
    fn meta_mirrors_the_indexed_fields() {
        let now = Timestamp::from_secs(100);
        let mut job = Job::new(
            id(7),
            JobSpec::new("q", "k")
                .priority(3)
                .tenant("acme".parse().expect("tenant")),
            now,
        );
        let meta = Meta::of(&job);
        assert_eq!(meta.state, JobState::Ready);
        assert_eq!(meta.tenant.as_ref().map(TenantId::as_str), Some("acme"));
        assert_eq!(meta.ready_key(job.id), ReadyKey::new(3, now, job.id));
        assert_eq!(meta.lease_deadline(), Timestamp::MIN);

        job.transition(JobState::Running, now)
            .expect("ready -> running");
        let token = LeaseToken::new(9);
        job.lease = Some(Lease::new(
            WorkerId::from_static("w1"),
            token,
            now,
            Duration::from_secs(30),
        ));
        let running = Meta::of(&job);
        assert_eq!(running.lease_deadline(), Timestamp::from_secs(130));
        assert!(running.holds_lease(token, Timestamp::from_secs(129)));
        assert!(!running.holds_lease(token, Timestamp::from_secs(130)));
        assert!(!running.holds_lease(LeaseToken::new(8), now));
        assert!(!running.holds_lease(LeaseToken::REDACTED, now));
    }
}
