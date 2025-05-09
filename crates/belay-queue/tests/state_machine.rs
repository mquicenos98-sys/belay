//! The job state machine as the engine implements it: every operation in
//! every state, then the finer points of each transition.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_common::BackoffPolicy;
use belay_json::json;
use belay_proto::api::{CompleteRequest, ErrorCode, FailRequest, HeartbeatRequest};
use belay_proto::{JobId, JobSpec, JobState, LeaseToken, Progress};
use belay_queue::Error;
use support::*;

const ALL: [JobState; 7] = JobState::ALL;

/// Operations applied to a job in the exhaustive table.
#[derive(Clone, Copy, Debug)]
enum Op {
    Lease,
    Heartbeat,
    Complete,
    FailRetryable,
    FailFatal,
    Cancel,
    Retry,
    Promote,
    Delete,
    /// A minute passes and the engine ticks.
    Tick,
}

const OPS: [Op; 10] = [
    Op::Lease,
    Op::Heartbeat,
    Op::Complete,
    Op::FailRetryable,
    Op::FailFatal,
    Op::Cancel,
    Op::Retry,
    Op::Promote,
    Op::Delete,
    Op::Tick,
];

#[derive(Debug, PartialEq)]
enum Want {
    /// The operation succeeds and the job ends up in this state.
    Now(JobState),
    /// The job no longer exists.
    Gone,
    /// The operation fails with this code and the job is unchanged.
    Fails(ErrorCode),
}

/// What each operation does in each state. Written out by hand, as the
/// specification the engine is checked against.
fn expected(state: JobState, op: Op) -> Want {
    use JobState::*;
    use Want::*;
    let worker_call = |on_running: JobState| match state {
        Running => Now(on_running),
        _ => Fails(ErrorCode::LeaseLost),
    };
    match op {
        // Only a ready job is leasable; everything else stays put.
        Op::Lease => Now(if state == Ready { Running } else { state }),
        Op::Heartbeat => worker_call(Running),
        Op::Complete => worker_call(Succeeded),
        Op::FailRetryable => worker_call(Retrying),
        Op::FailFatal => worker_call(Dead),
        Op::Cancel => match state {
            Scheduled | Ready | Retrying | Cancelled => Now(Cancelled),
            // Cancellation is requested; the worker acknowledges later.
            Running => Now(Running),
            Succeeded | Dead => Fails(ErrorCode::FailedPrecondition),
        },
        Op::Retry => match state {
            Dead | Cancelled => Now(Ready),
            _ => Fails(ErrorCode::FailedPrecondition),
        },
        Op::Promote => match state {
            Scheduled | Retrying | Ready => Now(Ready),
            _ => Fails(ErrorCode::FailedPrecondition),
        },
        Op::Delete => match state {
            Succeeded | Dead | Cancelled => Gone,
            _ => Fails(ErrorCode::FailedPrecondition),
        },
        Op::Tick => match state {
            // Timers fire within the minute; the lease (30s) runs out and
            // the retry waits out its 10s backoff from then.
            Scheduled | Ready => Now(Ready),
            Retrying => Now(Ready),
            Running => Now(Retrying),
            terminal => Now(terminal),
        },
    }
}

fn spec() -> JobSpec {
    JobSpec::new("q", "k")
        .max_attempts(3)
        .retry(BackoffPolicy::constant(Duration::from_secs(10)))
}

/// Puts a job into `state` through the public API. Returns its id and the
/// last lease token it had, if any.
fn job_in(h: &Harness, state: JobState) -> (JobId, Option<LeaseToken>) {
    match state {
        JobState::Scheduled => (h.enqueue(spec().delay(Duration::from_secs(45))), None),
        JobState::Ready => (h.enqueue(spec()), None),
        JobState::Cancelled => {
            let id = h.enqueue(spec());
            h.engine.cancel(&id).expect("cancel");
            (id, None)
        }
        leased => {
            let id = h.enqueue(spec());
            let l = h.lease_one("q");
            match leased {
                JobState::Running => {}
                JobState::Retrying => assert_eq!(h.fail(&l, true), JobState::Retrying),
                JobState::Dead => assert_eq!(h.fail(&l, false), JobState::Dead),
                JobState::Succeeded => h.complete(&l),
                other => unreachable!("{other} is handled above"),
            }
            (id, Some(l.token))
        }
    }
}

fn apply(h: &Harness, id: JobId, token: LeaseToken, op: Op) -> Result<(), Error> {
    let e = &h.engine;
    match op {
        Op::Lease => e
            .lease(&belay_proto::api::LeaseRequest::new(w("w2"), vec![q("q")]))
            .map(drop),
        Op::Heartbeat => e.heartbeat(&id, &HeartbeatRequest::new(token)).map(drop),
        Op::Complete => e.complete(&id, &CompleteRequest::new(token)),
        Op::FailRetryable => e.fail(&id, &FailRequest::new(token, "try again")).map(drop),
        Op::FailFatal => {
            let mut req = FailRequest::new(token, "fatal");
            req.retryable = false;
            e.fail(&id, &req).map(drop)
        }
        Op::Cancel => e.cancel(&id).map(drop),
        Op::Retry => e.retry(&id).map(drop),
        Op::Promote => e.promote(&id).map(drop),
        Op::Delete => e.delete(&id),
        Op::Tick => {
            h.secs(60);
            e.tick(h.now()).map(drop)
        }
    }
}

#[test]
fn every_operation_in_every_state_does_what_the_table_says() {
    for state in ALL {
        for op in OPS {
            let h = Harness::new();
            let (id, token) = job_in(&h, state);
            assert_eq!(h.state(id), state, "setup for {state}");
            let token = token.unwrap_or(LeaseToken::new(0xdead_beef));
            let outcome = apply(&h, id, token, op);
            let got = match (&outcome, h.engine.get(&id)) {
                (Err(e), _) => Want::Fails(e.code()),
                (Ok(()), Ok(job)) => Want::Now(job.state),
                (Ok(()), Err(Error::JobNotFound(_))) => Want::Gone,
                (Ok(()), Err(e)) => panic!("get failed after {op:?} in {state}: {e}"),
            };
            assert_eq!(got, expected(state, op), "{op:?} on a {state} job");
            if outcome.is_err() {
                assert_eq!(h.state(id), state, "a refused {op:?} changed a {state} job");
            }
            h.assert_consistent();
        }
    }
}

#[test]
fn the_table_agrees_with_the_proto_transition_rules() {
    // Every successful state change in the table is one the state machine
    // allows; the engine never takes a shortcut.
    for state in ALL {
        for op in OPS {
            if let Want::Now(next) = expected(state, op) {
                if next != state {
                    assert!(
                        state.can_transition_to(next),
                        "{state} -> {next} via {op:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn attempts_count_up_and_errors_are_recorded() {
    let h = Harness::new();
    let id = h.enqueue(spec());
    for attempt in 1..=3u32 {
        let l = h.lease_one("q");
        assert_eq!(l.job.attempt, attempt);
        assert_eq!(l.job.started_at, Some(h.now()));
        let want = if attempt < 3 {
            JobState::Retrying
        } else {
            JobState::Dead
        };
        assert_eq!(h.fail(&l, true), want);
        h.secs(10);
        h.tick();
    }
    let job = h.job(id);
    assert_eq!(job.state, JobState::Dead);
    assert_eq!(job.attempts_left(), 0);
    let attempts: Vec<u32> = job.errors.iter().map(|e| e.attempt).collect();
    assert_eq!(attempts, [1, 2, 3]);
    assert!(job.errors.iter().all(|e| e.worker == Some(w("w1"))));
    assert_eq!(job.finished_at, Some(start() + Duration::from_secs(20)));
}

#[test]
fn retry_waits_for_the_backoff_delay() {
    let h = Harness::new();
    let id = h.enqueue(spec());
    let l = h.lease_one("q");
    h.fail(&l, true);
    let job = h.job(id);
    assert_eq!(job.run_at, h.now() + Duration::from_secs(10));
    h.secs(9);
    assert!(h.lease("q", 1).is_empty(), "still backing off");
    h.secs(1);
    // Leasing promotes due jobs itself; no tick needed.
    assert_eq!(h.lease_one("q").job.attempt, 2);
}

#[test]
fn retry_in_overrides_the_backoff_policy() {
    let h = Harness::new();
    let id = h.enqueue(spec());
    let l = h.lease_one("q");
    let mut req = FailRequest::new(l.token, "rate limited");
    req.retry_in = Some(Duration::from_secs(300));
    req.kind = Some("http_429".into());
    assert_eq!(h.engine.fail(&id, &req).expect("fail"), JobState::Retrying);
    let job = h.job(id);
    assert_eq!(job.run_at, h.now() + Duration::from_secs(300));
    assert_eq!(
        job.last_error().and_then(|e| e.kind.as_deref()),
        Some("http_429")
    );
}

#[test]
fn queue_default_backoff_applies_to_jobs_without_their_own() {
    let h = Harness::new();
    let mut config = belay_proto::QueueConfig::default();
    config.default_retry = BackoffPolicy::constant(Duration::from_secs(7));
    config.default_max_attempts = 2;
    h.engine.set_config(&q("q"), config).expect("config");
    let id = h.enqueue(JobSpec::new("q", "k"));
    let job = h.job(id);
    assert_eq!(
        job.spec.max_attempts,
        Some(2),
        "defaults are filled at enqueue"
    );
    let l = h.lease_one("q");
    h.fail(&l, true);
    assert_eq!(h.job(id).run_at, h.now() + Duration::from_secs(7));
    h.secs(7);
    let l = h.lease_one("q");
    assert_eq!(h.fail(&l, true), JobState::Dead, "two attempts allowed");
}

#[test]
fn non_retryable_failures_die_with_attempts_left() {
    let h = Harness::new();
    let id = h.enqueue(spec().max_attempts(10));
    let l = h.lease_one("q");
    assert_eq!(h.fail(&l, false), JobState::Dead);
    let job = h.job(id);
    assert_eq!(job.attempts_left(), 9);
    assert!(!job.errors[0].retryable);
}

#[test]
fn cancelling_a_running_job_is_a_request_the_worker_acknowledges() {
    let h = Harness::new();
    let id = h.enqueue(spec());
    let l = h.lease_one("q");
    let job = h.engine.cancel(&id).expect("cancel");
    assert_eq!((job.state, job.cancel_requested), (JobState::Running, true));
    assert!(h.heartbeat(&l).cancel_requested);
    // Asking again changes nothing.
    assert!(h.engine.cancel(&id).expect("cancel again").cancel_requested);
    assert_eq!(h.fail(&l, true), JobState::Cancelled);
    let job = h.job(id);
    assert!(!job.cancel_requested, "cleared on leaving running");
    assert!(job.lease.is_none());
}

#[test]
fn a_job_that_completes_despite_a_cancel_request_succeeds() {
    let h = Harness::new();
    let id = h.enqueue(spec());
    let l = h.lease_one("q");
    h.engine.cancel(&id).expect("cancel");
    h.complete(&l);
    assert_eq!(h.state(id), JobState::Succeeded);
}

#[test]
fn a_cancel_requested_job_whose_lease_expires_is_cancelled() {
    let h = Harness::new();
    let id = h.enqueue(spec());
    h.lease_one("q");
    h.engine.cancel(&id).expect("cancel");
    h.secs(30);
    let report = h.tick();
    assert_eq!(report.expired, 1);
    assert_eq!(h.state(id), JobState::Cancelled);
}
