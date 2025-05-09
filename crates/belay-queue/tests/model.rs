//! Randomized model-based test: thousands of seeded random operations are
//! applied both to the engine and to a deliberately simple reference
//! model, and the two must agree after every step, across restarts and
//! simulated power losses.
//!
//! The model keeps jobs in a map and answers every question by brute
//! force (sorting all ready jobs to find the next lease, scanning all
//! claims), so it is easy to trust. Jobs carry no tenant and queues no
//! limits, which makes the engine's choices fully predictable: every
//! lease must hand out exactly the best ready jobs of each queue.

mod support;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use belay_common::{BackoffPolicy, Rng, Timestamp};
use belay_proto::api::{
    CompleteRequest, EnqueueStatus, FailRequest, HeartbeatRequest, LeaseRequest,
};
use belay_proto::{JobId, JobSpec, JobState, LeaseToken, OnConflict, Unique, UniqueScope};
use belay_queue::{EngineOptions, Error};
use belay_store::vfs::{Crash, MemFs};
use support::*;

const QUEUES: [&str; 3] = ["a", "b", "c"];
const LEASE: Duration = Duration::from_secs(30);
const BACKOFF: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
struct MJob {
    queue: &'static str,
    priority: i16,
    run_at: Timestamp,
    state: JobState,
    attempt: u32,
    max_attempts: u32,
    token: Option<LeaseToken>,
    expires: Timestamp,
    heartbeat: Timestamp,
    cancel_requested: bool,
    unique: Option<String>,
}

#[derive(Debug, Default)]
struct Model {
    jobs: BTreeMap<JobId, MJob>,
    /// (queue, key) -> holder, while the holder is unfinished.
    claims: HashMap<(&'static str, String), JobId>,
    paused: BTreeSet<&'static str>,
}

impl Model {
    fn release_claim(&mut self, id: JobId) {
        self.claims.retain(|_, holder| *holder != id);
    }

    fn finish(&mut self, id: JobId, state: JobState) {
        let job = self.jobs.get_mut(&id).expect("model job");
        job.state = state;
        job.token = None;
        job.cancel_requested = false;
        self.release_claim(id);
    }

    /// Scheduled and retrying jobs of `queues` whose time has come.
    fn promote(&mut self, queues: &[&'static str], now: Timestamp) -> u64 {
        let mut n = 0;
        for job in self.jobs.values_mut() {
            if queues.contains(&job.queue) && job.state.is_waiting() && job.run_at <= now {
                job.state = JobState::Ready;
                n += 1;
            }
        }
        n
    }

    /// Ready jobs of `queue` in the order the engine must hand them out.
    fn ready(&self, queue: &str) -> Vec<JobId> {
        let mut ready: Vec<(i16, Timestamp, JobId)> = self
            .jobs
            .iter()
            .filter(|(_, j)| j.queue == queue && j.state == JobState::Ready)
            .map(|(id, j)| (-j.priority, j.run_at, *id))
            .collect();
        ready.sort();
        ready.into_iter().map(|(_, _, id)| id).collect()
    }

    /// A failed attempt of a running job, as `fail` or lease expiry does it.
    fn fail(&mut self, id: JobId, retryable: bool, now: Timestamp) -> JobState {
        let job = self.jobs.get(&id).expect("model job");
        let next = if job.cancel_requested {
            JobState::Cancelled
        } else if retryable && job.attempt < job.max_attempts {
            JobState::Retrying
        } else {
            JobState::Dead
        };
        if next == JobState::Retrying {
            let job = self.jobs.get_mut(&id).expect("model job");
            job.state = JobState::Retrying;
            job.run_at = now + BACKOFF;
            job.token = None;
            job.cancel_requested = false;
        } else {
            self.finish(id, next);
        }
        next
    }

    fn holds_lease(&self, id: JobId, token: LeaseToken, now: Timestamp) -> bool {
        self.jobs.get(&id).is_some_and(|j| {
            j.state == JobState::Running && j.token == Some(token) && now < j.expires
        })
    }
}

struct Run {
    h: Harness,
    fs: MemFs,
    model: Model,
    rng: Rng,
    ops: BTreeMap<&'static str, u64>,
}

impl Run {
    fn new(seed: u64) -> Run {
        let fs = MemFs::new();
        let h = Harness::on(durable(&fs), EngineOptions::new().batch_size(1000));
        Run {
            h,
            fs,
            model: Model::default(),
            rng: Rng::seed_from_u64(seed),
            ops: BTreeMap::new(),
        }
    }

    fn now(&self) -> Timestamp {
        self.h.now()
    }

    fn pick_queue(&mut self) -> &'static str {
        QUEUES[self.rng.gen_range(0..QUEUES.len() as u64) as usize]
    }

    fn pick_job(&mut self, filter: impl Fn(&MJob) -> bool) -> Option<JobId> {
        let ids: Vec<JobId> = self
            .model
            .jobs
            .iter()
            .filter(|(_, j)| filter(j))
            .map(|(id, _)| *id)
            .collect();
        self.rng.choose(&ids).copied()
    }

    /// A running job with its token, or now and then a job with a token
    /// that is not (or no longer) valid.
    fn pick_lease(&mut self) -> Option<(JobId, LeaseToken)> {
        if self.rng.gen_bool(0.2) {
            let id = self.pick_job(|_| true)?;
            let token = match self.model.jobs[&id].token {
                Some(t) if self.rng.gen_bool(0.5) => LeaseToken::new(t.as_u64() ^ 0xff),
                _ => LeaseToken::new(self.rng.next_u64() | 1),
            };
            return Some((id, token));
        }
        let id = self.pick_job(|j| j.state == JobState::Running)?;
        Some((
            id,
            self.model.jobs[&id]
                .token
                .expect("running jobs have a token"),
        ))
    }

    fn count(&mut self, op: &'static str) {
        *self.ops.entry(op).or_default() += 1;
    }

    fn step(&mut self) {
        match self.rng.gen_range(0..100u64) {
            0..=24 => self.enqueue(),
            25..=44 => self.lease(),
            45..=51 => self.heartbeat(),
            52..=61 => self.complete(),
            62..=71 => self.fail(),
            72..=75 => self.cancel(),
            76..=78 => self.retry(),
            79..=81 => self.promote(),
            82..=84 => self.delete(),
            85..=87 => self.pause_or_resume(),
            88..=93 => {
                self.count("advance");
                let d = Duration::from_millis(self.rng.gen_range(100..20_000u64));
                self.h.advance(d);
            }
            94..=97 => self.tick(),
            98 => self.purge(),
            _ => self.restart(),
        }
    }

    fn enqueue(&mut self) {
        self.count("enqueue");
        let now = self.now();
        let queue = self.pick_queue();
        let priority = self.rng.gen_range(0..3u64) as i16 - 1;
        let max_attempts = self.rng.gen_range(1..4u64) as u32;
        let mut spec = JobSpec::new(queue, "k")
            .priority(priority)
            .max_attempts(max_attempts)
            .retry(BackoffPolicy::constant(BACKOFF));
        let delay = if self.rng.gen_bool(0.3) {
            Duration::from_millis(self.rng.gen_range(1..30_000u64))
        } else {
            Duration::ZERO
        };
        if !delay.is_zero() {
            spec = spec.delay(delay);
        }
        let mut unique = None;
        if self.rng.gen_bool(0.3) {
            let key = format!("k{}", self.rng.gen_range(0..3u64));
            let policy = if self.rng.gen_bool(0.3) {
                OnConflict::Reject
            } else {
                OnConflict::ReturnExisting
            };
            spec = spec.unique_with(
                Unique::new(key.clone(), UniqueScope::UntilFinished).on_conflict(policy),
            );
            unique = Some((key, policy));
        }

        let result = self.h.engine.enqueue(spec);
        if let Some((key, policy)) = &unique {
            if let Some(&holder) = self.model.claims.get(&(queue, key.clone())) {
                let holder_state = self.model.jobs[&holder].state;
                match (policy, result) {
                    (OnConflict::Reject, Err(Error::UniqueConflict { holder: got, .. })) => {
                        assert_eq!(got, holder);
                    }
                    (OnConflict::ReturnExisting, Ok(r)) => {
                        assert_eq!(
                            (r.id, r.status, r.state),
                            (holder, EnqueueStatus::Duplicate, holder_state)
                        );
                    }
                    (policy, other) => panic!("{policy} conflict gave {other:?}"),
                }
                return;
            }
        }
        let result = result.expect("enqueue without conflict");
        assert_eq!(result.status, EnqueueStatus::Created);
        let state = if delay.is_zero() {
            JobState::Ready
        } else {
            JobState::Scheduled
        };
        assert_eq!(result.state, state);
        if let Some((key, _)) = unique.clone() {
            self.model.claims.insert((queue, key), result.id);
        }
        self.model.jobs.insert(
            result.id,
            MJob {
                queue,
                priority,
                run_at: now + delay,
                state,
                attempt: 0,
                max_attempts,
                token: None,
                expires: now,
                heartbeat: now,
                cancel_requested: false,
                unique: unique.map(|(key, _)| key),
            },
        );
    }

    fn lease(&mut self) {
        self.count("lease");
        let now = self.now();
        let mut queues: Vec<&'static str> = QUEUES
            .iter()
            .copied()
            .filter(|_| self.rng.gen_bool(0.6))
            .collect();
        if queues.is_empty() {
            queues.push(self.pick_queue());
        }
        self.rng.shuffle(&mut queues);
        let max = self.rng.gen_range(1..4u64) as u32;
        let worker = ["w1", "w2", "w3"][self.rng.gen_range(0..3u64) as usize];
        let req = LeaseRequest::new(w(worker), queues.iter().map(|name| q(name)).collect())
            .max_jobs(max)
            .lease_for(LEASE);
        let leased = self.h.engine.lease(&req).expect("lease");

        self.model.promote(&queues, now);
        let available: usize = queues
            .iter()
            .filter(|name| !self.model.paused.contains(*name))
            .map(|name| self.model.ready(name).len())
            .sum();
        assert_eq!(leased.len(), available.min(max as usize), "jobs handed out");
        for name in &queues {
            let got: Vec<JobId> = leased
                .iter()
                .filter(|l| l.job.spec.queue == *name)
                .map(|l| l.job.id)
                .collect();
            let best = self.model.ready(name);
            assert_eq!(got, best[..got.len()], "queue {name} gave the wrong jobs");
        }
        for l in &leased {
            let job = self
                .model
                .jobs
                .get_mut(&l.job.id)
                .expect("leased job is known");
            job.state = JobState::Running;
            job.attempt += 1;
            job.token = Some(l.token);
            job.expires = now + LEASE;
            job.heartbeat = now;
            assert_eq!(l.job.attempt, job.attempt);
        }
    }

    fn heartbeat(&mut self) {
        self.count("heartbeat");
        let Some((id, token)) = self.pick_lease() else {
            return;
        };
        let now = self.now();
        let valid = self.model.holds_lease(id, token, now);
        let result = self.h.engine.heartbeat(&id, &HeartbeatRequest::new(token));
        if valid {
            let resp = result.expect("heartbeat on a held lease");
            let job = self.model.jobs.get_mut(&id).expect("model job");
            let by = job.expires.saturating_duration_since(job.heartbeat);
            job.expires = job.expires.max(now + by);
            job.heartbeat = now;
            assert_eq!(resp.expires_at, job.expires);
            assert_eq!(resp.cancel_requested, job.cancel_requested);
        } else {
            assert!(matches!(result, Err(Error::LeaseLost(_))), "{result:?}");
        }
    }

    fn complete(&mut self) {
        self.count("complete");
        let Some((id, token)) = self.pick_lease() else {
            return;
        };
        let valid = self.model.holds_lease(id, token, self.now());
        let result = self.h.engine.complete(&id, &CompleteRequest::new(token));
        if valid {
            result.expect("complete on a held lease");
            self.model.finish(id, JobState::Succeeded);
        } else {
            assert!(matches!(result, Err(Error::LeaseLost(_))), "{result:?}");
        }
    }

    fn fail(&mut self) {
        self.count("fail");
        let Some((id, token)) = self.pick_lease() else {
            return;
        };
        let now = self.now();
        let valid = self.model.holds_lease(id, token, now);
        let mut req = FailRequest::new(token, "boom");
        req.retryable = self.rng.gen_bool(0.7);
        let result = self.h.engine.fail(&id, &req);
        if valid {
            let got = result.expect("fail on a held lease");
            assert_eq!(got, self.model.fail(id, req.retryable, now));
        } else {
            assert!(matches!(result, Err(Error::LeaseLost(_))), "{result:?}");
        }
    }

    fn cancel(&mut self) {
        self.count("cancel");
        let Some(id) = self.pick_job(|_| true) else {
            return;
        };
        let result = self.h.engine.cancel(&id);
        let job = self.model.jobs.get_mut(&id).expect("model job");
        match job.state {
            JobState::Scheduled | JobState::Ready | JobState::Retrying => {
                result.expect("cancel");
                self.model.finish(id, JobState::Cancelled);
            }
            JobState::Running => {
                assert!(result.expect("cancel").cancel_requested);
                job.cancel_requested = true;
            }
            JobState::Cancelled => assert!(result.is_ok()),
            _ => assert!(matches!(result, Err(Error::WrongState { .. }))),
        }
    }

    fn retry(&mut self) {
        self.count("retry");
        let Some(id) = self.pick_job(|j| j.state.is_terminal() || j.state == JobState::Ready)
        else {
            return;
        };
        let now = self.now();
        let result = self.h.engine.retry(&id);
        let job = self.model.jobs.get_mut(&id).expect("model job");
        if matches!(job.state, JobState::Dead | JobState::Cancelled) {
            result.expect("retry");
            job.state = JobState::Ready;
            job.attempt = 0;
            job.run_at = now;
        } else {
            assert!(matches!(result, Err(Error::WrongState { .. })));
        }
    }

    fn promote(&mut self) {
        self.count("promote");
        let Some(id) = self.pick_job(|_| true) else {
            return;
        };
        let now = self.now();
        let result = self.h.engine.promote(&id);
        let job = self.model.jobs.get_mut(&id).expect("model job");
        match job.state {
            JobState::Scheduled | JobState::Retrying => {
                result.expect("promote");
                job.state = JobState::Ready;
                job.run_at = now;
            }
            JobState::Ready => assert!(result.is_ok()),
            _ => assert!(matches!(result, Err(Error::WrongState { .. }))),
        }
    }

    fn delete(&mut self) {
        self.count("delete");
        let Some(id) = self.pick_job(|_| true) else {
            return;
        };
        let result = self.h.engine.delete(&id);
        if self.model.jobs[&id].state.is_terminal() {
            result.expect("delete");
            self.model.jobs.remove(&id);
        } else {
            assert!(matches!(result, Err(Error::WrongState { .. })));
        }
    }

    fn pause_or_resume(&mut self) {
        self.count("pause");
        let queue = self.pick_queue();
        if self.model.paused.contains(queue) {
            self.h.engine.resume(&q(queue)).expect("resume");
            self.model.paused.remove(queue);
        } else {
            self.h.engine.pause(&q(queue)).expect("pause");
            self.model.paused.insert(queue);
        }
    }

    fn tick(&mut self) {
        self.count("tick");
        let now = self.now();
        let report = self.h.tick();
        let promoted = self.model.promote(&QUEUES, now);
        let expired: Vec<JobId> = self
            .model
            .jobs
            .iter()
            .filter(|(_, j)| j.state == JobState::Running && j.expires <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in &expired {
            self.model.fail(*id, true, now);
        }
        assert_eq!(report.promoted, promoted, "promoted by tick");
        assert_eq!(report.expired, expired.len() as u64, "expired by tick");
        assert_eq!(report.deleted, 0, "nothing is old enough for retention");
    }

    fn purge(&mut self) {
        self.count("purge");
        let queue = self.pick_queue();
        let doomed: Vec<JobId> = self
            .model
            .jobs
            .iter()
            .filter(|(_, j)| j.queue == queue && j.state == JobState::Ready)
            .map(|(id, _)| *id)
            .collect();
        match self.h.engine.purge(&q(queue), Some(JobState::Ready)) {
            Ok(n) => assert_eq!(n, doomed.len() as u64),
            Err(Error::QueueNotFound(_)) => {
                assert!(doomed.is_empty());
                return;
            }
            Err(e) => panic!("purge failed: {e}"),
        }
        for id in doomed {
            self.model.jobs.remove(&id);
            self.model.release_claim(id);
        }
    }

    fn restart(&mut self) {
        self.count("restart");
        if self.rng.gen_bool(0.5) {
            self.fs.crash(&Crash::PowerLoss);
        }
        let fs = self.fs.clone();
        self.h.restart(|| durable(&fs));
        self.check_all();
    }

    /// Compares every job and queue with the model.
    fn check_all(&self) {
        let jobs = self.h.all_jobs();
        let ids: Vec<JobId> = jobs.iter().map(|j| j.id).collect();
        let model_ids: Vec<JobId> = self.model.jobs.keys().copied().collect();
        assert_eq!(ids, model_ids, "the set of jobs");
        for job in &jobs {
            let m = &self.model.jobs[&job.id];
            assert_eq!(
                (
                    job.state,
                    job.attempt,
                    job.spec.queue.as_str(),
                    job.spec.priority
                ),
                (m.state, m.attempt, m.queue, m.priority),
                "job {}",
                job.id
            );
            assert_eq!(job.cancel_requested, m.cancel_requested, "job {}", job.id);
            assert_eq!(job.spec.unique.as_ref().map(|u| u.key.clone()), m.unique);
            if job.state.is_waiting() || job.state == JobState::Ready {
                assert_eq!(
                    job.run_at, m.run_at,
                    "run_at of {} job {}",
                    job.state, job.id
                );
            }
            if job.state == JobState::Running {
                assert_eq!(job.lease.as_ref().map(|l| l.expires_at), Some(m.expires));
            }
        }
        for info in self.h.engine.list_queues() {
            let name = info.name.as_str();
            assert_eq!(
                info.paused,
                self.model.paused.contains(name),
                "queue {name}"
            );
            for state in JobState::ALL {
                let expected = self
                    .model
                    .jobs
                    .values()
                    .filter(|j| j.queue == name && j.state == state)
                    .count() as u64;
                assert_eq!(info.counts.get(state), expected, "{state} jobs in {name}");
            }
        }
        let claims: usize = self
            .h
            .engine
            .stats()
            .queues
            .iter()
            .map(|s| s.unique_keys)
            .sum();
        assert_eq!(claims, self.model.claims.len(), "claimed keys");
        self.h.assert_consistent();
    }
}

fn run(seed: u64, steps: usize) {
    let mut run = Run::new(seed);
    for step in 0..steps {
        run.step();
        if step % 25 == 24 {
            run.check_all();
        }
    }
    run.check_all();
    // Every kind of operation was exercised.
    for op in [
        "enqueue",
        "lease",
        "heartbeat",
        "complete",
        "fail",
        "cancel",
        "retry",
        "promote",
        "delete",
        "pause",
        "advance",
        "tick",
        "purge",
        "restart",
    ] {
        assert!(
            run.ops.get(op).copied().unwrap_or(0) > 0,
            "seed {seed} never ran {op}"
        );
    }
}

#[test]
fn engine_agrees_with_the_model_seed_1() {
    run(1, 2000);
}

#[test]
fn engine_agrees_with_the_model_seed_2() {
    run(2, 1500);
}

#[test]
fn engine_agrees_with_the_model_seed_3() {
    run(3, 1500);
}
