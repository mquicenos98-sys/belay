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
