//! Uniqueness keys: scopes, conflict policies, batches, and how claims
//! survive deletion and restarts.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use belay_proto::api::{ApiError, EnqueueStatus, ErrorCode};
use belay_proto::{JobId, JobSpec, JobState, OnConflict, Unique, UniqueScope};
use belay_queue::Error;
use support::*;

fn keyed(key: &str, scope: UniqueScope, on_conflict: OnConflict) -> JobSpec {
    JobSpec::new("q", "k").unique_with(Unique::new(key, scope).on_conflict(on_conflict))
}

fn until_finished(key: &str) -> JobSpec {
    keyed(key, UniqueScope::UntilFinished, OnConflict::ReturnExisting)
}

#[test]
fn a_held_key_returns_the_existing_job() {
    let h = Harness::new();
    let first = h.engine.enqueue(until_finished("user:1")).expect("enqueue");
    assert_eq!(first.status, EnqueueStatus::Created);
    let again = h.engine.enqueue(until_finished("user:1")).expect("enqueue");
    assert_eq!(
        (again.id, again.status, again.state),
        (first.id, EnqueueStatus::Duplicate, JobState::Ready)
    );
    assert_eq!(h.all_jobs().len(), 1);
    assert_eq!(h.engine.stats().counters.duplicates, 1);
    assert_eq!(
        h.events(),
        ["job.enqueued"],
        "a duplicate is not an enqueue"
    );
}

#[test]
fn until_finished_holds_through_retries_and_releases_at_the_end() {
    let h = Harness::new();
    let id = h.enqueue(until_finished("k").max_attempts(2));
    let l = h.lease_one("q");
    h.fail(&l, true);
    let dup = h.engine.enqueue(until_finished("k")).expect("enqueue");
    assert_eq!((dup.id, dup.state), (id, JobState::Retrying));
    h.secs(3600);
    let l = h.lease_one("q");
    assert_eq!(
        h.engine.enqueue(until_finished("k")).expect("enqueue").id,
        id
    );
    h.complete(&l);
    let next = h.engine.enqueue(until_finished("k")).expect("enqueue");
    assert_eq!(next.status, EnqueueStatus::Created);
    assert_ne!(next.id, id);
}

#[test]
fn until_started_releases_when_the_job_is_leased() {
    let h = Harness::new();
    let spec = || keyed("k", UniqueScope::UntilStarted, OnConflict::ReturnExisting);
    let id = h.enqueue(spec().delay(Duration::from_secs(60)));
    assert_eq!(h.engine.enqueue(spec()).expect("enqueue").id, id);
    h.engine.promote(&id).expect("promote");
    assert_eq!(
        h.engine.enqueue(spec()).expect("enqueue").id,
        id,
        "ready still holds"
    );
    let l = h.lease_one("q");
    assert_eq!(l.job.id, id);
    let second = h.engine.enqueue(spec()).expect("enqueue");
    assert_eq!(second.status, EnqueueStatus::Created);
    // The running job does not take its key back when it fails and
    // becomes ready again.
    h.fail(&l, true);
    let third = h.engine.enqueue(spec()).expect("enqueue");
    assert_eq!(third.id, second.id);
}

#[test]
fn timed_keys_outlive_the_job_until_their_period_ends() {
    let h = Harness::new();
    let hour = UniqueScope::For(Duration::from_secs(3600));
    let spec = || keyed("digest", hour, OnConflict::ReturnExisting);
    let id = h.enqueue(spec());
    let l = h.lease_one("q");
    h.complete(&l);
    h.engine.delete(&id).expect("delete");
    let dup = h.engine.enqueue(spec()).expect("enqueue");
    assert_eq!(
        (dup.id, dup.status, dup.state),
        (id, EnqueueStatus::Duplicate, JobState::Succeeded),
        "the deleted holder's final state is remembered"
    );
    h.secs(3599);
    assert_eq!(h.tick().claims_released, 0);
    h.secs(1);
    assert_eq!(h.tick().claims_released, 1);
    let fresh = h.engine.enqueue(spec()).expect("enqueue");
    assert_eq!(fresh.status, EnqueueStatus::Created);
    h.assert_consistent();
}

#[test]
fn an_expired_timed_key_is_free_before_the_tick_sweeps_it() {
    let h = Harness::new();
    let spec = || {
        keyed(
            "k",
            UniqueScope::For(Duration::from_secs(10)),
            OnConflict::Reject,
        )
    };
    let first = h.enqueue(spec());
    h.secs(10);
    let second = h.engine.enqueue(spec()).expect("expired claim is free");
    assert_ne!(second.id, first);
    // The sweep then finds the new claim unexpired and leaves it alone.
    assert_eq!(h.tick().claims_released, 0);
    assert!(h.engine.enqueue(spec()).is_err());
}

#[test]
fn reject_fails_with_already_exists() {
    let h = Harness::new();
    let spec = || keyed("invoice:7", UniqueScope::UntilFinished, OnConflict::Reject);
    let id = h.enqueue(spec());
    let err = h.engine.enqueue(spec()).unwrap_err();
    assert!(
        matches!(&err, Error::UniqueConflict { key, holder, .. } if key == "invoice:7" && *holder == id)
    );
    let api = ApiError::from(err);
    assert_eq!(api.code, ErrorCode::AlreadyExists);
    assert_eq!(api.http_status(), 409);
}

#[test]
fn replace_cancels_a_pending_holder() {
    let h = Harness::new();
    let finished = Arc::new(Mutex::new(Vec::new()));
    {
        let finished = Arc::clone(&finished);
        h.engine
            .on_finished(move |job| finished.lock().unwrap().push(job.id));
    }
    let spec = |payload: i64| {
        keyed("profile:9", UniqueScope::UntilFinished, OnConflict::Replace)
            .payload(belay_json::json!({ "version": payload }))
    };
    let old = h.enqueue(spec(1).delay(Duration::from_secs(30)));
    h.sink.take();
    let new = h.engine.enqueue(spec(2)).expect("enqueue");
    assert_eq!(new.status, EnqueueStatus::Replaced);
    assert_eq!(h.state(old), JobState::Cancelled);
    assert_eq!(h.events(), ["job.cancelled", "job.enqueued"]);
    assert_eq!(*finished.lock().unwrap(), [old]);
    let l = h.lease_one("q");
    assert_eq!(l.job.spec.payload, belay_json::json!({"version": 2}));
    assert_eq!(h.engine.stats().counters.replaced, 1);
}

#[test]
fn replace_leaves_a_running_holder_alone() {
    let h = Harness::new();
    let spec = || keyed("k", UniqueScope::UntilFinished, OnConflict::Replace);
    let id = h.enqueue(spec());
    h.lease_one("q");
    let result = h.engine.enqueue(spec()).expect("enqueue");
    assert_eq!(
        (result.id, result.status, result.state),
        (id, EnqueueStatus::Duplicate, JobState::Running)
    );
}

#[test]
fn replace_takes_the_key_of_a_purged_pending_holder() {
    let h = Harness::new();
    let spec = || {
        keyed(
            "k",
            UniqueScope::For(Duration::from_secs(600)),
            OnConflict::Replace,
        )
    };
    let old = h.enqueue(spec());
    assert_eq!(h.engine.purge(&q("q"), None).expect("purge"), 1);
    let dup = h
        .engine
        .enqueue(keyed(
            "k",
            UniqueScope::For(Duration::from_secs(600)),
            OnConflict::ReturnExisting,
        ))
        .expect("enqueue");
    assert_eq!((dup.id, dup.state), (old, JobState::Ready));
    let new = h.engine.enqueue(spec()).expect("enqueue");
    assert_eq!(new.status, EnqueueStatus::Replaced);
    assert_ne!(new.id, old);
    h.assert_consistent();
}

#[test]
fn keys_are_scoped_to_their_queue() {
    let h = Harness::new();
    let a = h.enqueue(until_finished("k"));
    let other = JobSpec::new("other", "k").unique("k", UniqueScope::UntilFinished);
    let b = h.engine.enqueue(other).expect("enqueue");
    assert_eq!(b.status, EnqueueStatus::Created);
    assert_ne!(a, b.id);
}

#[test]
fn a_batch_sees_its_own_claims() {
    let h = Harness::new();
    let results = h
        .engine
        .enqueue_batch(vec![
            until_finished("a"),
            until_finished("b"),
            until_finished("a"),
            keyed("b", UniqueScope::UntilFinished, OnConflict::Replace),
        ])
        .expect("batch");
    let statuses: Vec<EnqueueStatus> = results.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses,
        [
            EnqueueStatus::Created,
            EnqueueStatus::Created,
            EnqueueStatus::Duplicate,
            EnqueueStatus::Replaced
        ]
    );
    assert_eq!(results[2].id, results[0].id);
    assert_eq!(h.state(results[1].id), JobState::Cancelled);
    assert_eq!(h.all_jobs().len(), 3);
    h.assert_consistent();
}

#[test]
fn a_rejected_spec_fails_the_whole_batch() {
    let h = Harness::new();
    h.enqueue(keyed(
        "taken",
        UniqueScope::UntilFinished,
        OnConflict::Reject,
    ));
    let before = h.engine.stats();
    let err = h
        .engine
        .enqueue_batch(vec![
            JobSpec::new("q", "fine"),
            keyed("taken", UniqueScope::UntilFinished, OnConflict::Reject),
        ])
        .unwrap_err();
    assert!(matches!(err, Error::UniqueConflict { .. }));
    assert_eq!(h.all_jobs().len(), 1, "nothing from the batch was written");
    assert_eq!(h.engine.stats().counters, before.counters);
    h.assert_consistent();
}

#[test]
fn retrying_a_dead_job_does_not_reclaim_its_key() {
    let h = Harness::new();
    let id = h.enqueue(until_finished("k").max_attempts(1));
    let l = h.lease_one("q");
    h.fail(&l, true);
    let newer = h.enqueue(until_finished("k"));
    h.engine.retry(&id).expect("retry");
    assert_eq!(
        h.engine.enqueue(until_finished("k")).expect("enqueue").id,
        newer
    );
    // Finishing the retried job leaves the newer claim in place.
    let leased = h.lease("q", 2);
    for l in &leased {
        if l.job.id == id {
            h.complete(l);
        }
    }
    assert_eq!(
        h.engine.enqueue(until_finished("k")).expect("enqueue").id,
        newer
    );
    h.assert_consistent();
}

#[test]
fn claims_survive_a_restart() {
    let mut h = Harness::new();
    let held = h.enqueue(until_finished("held"));
    let timed = h.enqueue(keyed(
        "timed",
        UniqueScope::For(Duration::from_secs(60)),
        OnConflict::ReturnExisting,
    ));
    // Same priority and time: the older id goes first.
    for l in h.lease("q", 2) {
        h.complete(&l);
    }
    h.reopen();
    h.assert_consistent();
    let dup = h
        .engine
        .enqueue(keyed(
            "timed",
            UniqueScope::For(Duration::from_secs(60)),
            OnConflict::ReturnExisting,
        ))
        .expect("enqueue");
    assert_eq!(dup.id, timed, "timed claims hold after completion");
    let again = h.engine.enqueue(until_finished("held")).expect("enqueue");
    assert_eq!(
        again.status,
        EnqueueStatus::Created,
        "released on completion"
    );
    assert_ne!(again.id, held);
    h.secs(60);
    assert_eq!(h.tick().claims_released, 1);
}

#[test]
fn cancelled_holders_release_their_key() {
    let h = Harness::new();
    let id = h.enqueue(until_finished("k"));
    h.engine.cancel(&id).expect("cancel");
    let next = h.engine.enqueue(until_finished("k")).expect("enqueue");
    assert_eq!(next.status, EnqueueStatus::Created);
    let ids: Vec<JobId> = h.all_jobs().iter().map(|j| j.id).collect();
    assert_eq!(ids, [id, next.id]);
}

#[test]
fn queue_stats_count_claimed_keys() {
    let h = Harness::new();
    h.enqueue(until_finished("a"));
    h.enqueue(until_finished("b"));
    h.enqueue(JobSpec::new("q", "plain"));
    let stats = h.engine.stats();
    assert_eq!(stats.queue("q").map(|s| s.unique_keys), Some(2));
}
