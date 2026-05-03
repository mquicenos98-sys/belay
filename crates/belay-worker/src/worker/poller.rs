//! The poller: asks the source for as many jobs as can start, sorts what
//! comes back into "run now" and "wait for the kind's limit", and backs off
//! while the source is failing.
//!
//! The capacity rule lives in [`State::lease_capacity`]. Source errors
//! start a streak: the first failure is logged as a warning, later ones
//! at debug level, and the recovery once at info, so an outage of any
//! length costs two lines in the log instead of one per retry.
//!
//! [`State::lease_capacity`]: crate::worker::state::State::lease_capacity

use std::sync::Arc;
use std::time::{Duration, Instant};

use belay_common::rand::Rng;
use belay_common::sync::{lock, wait};
use belay_common::{debug, info, warn};
use belay_proto::api::{LeaseRequest, LeasedJob};

use crate::registry::unknown_kind;
use crate::stats::Counters;
use crate::worker::report::{settle, shutdown_error, Report};
use crate::worker::state::{Shared, Tracked};

/// Body of the poller thread.
pub(crate) fn run_poller(shared: &Shared) {
    let config = &shared.config;
    let mut rng = Rng::from_entropy();
    let mut outage: Option<(u32, Instant)> = None;
    while let Some(want) = wait_for_capacity(shared) {
        let mut req = LeaseRequest::new(config.id.clone(), config.queues.clone())
            .max_jobs(u32::try_from(want).unwrap_or(u32::MAX))
            .lease_for(config.lease_for);
        if !config.poll_wait.is_zero() {
            req = req.wait(config.poll_wait);
        }
        let asked = Instant::now();
        match shared.source.lease(&req) {
            Ok(jobs) => {
                if let Some((failures, since)) = outage.take() {
                    info!(
                        "job source reachable again";
                        failed_attempts = failures,
                        outage = since.elapsed(),
                    );
                }
                let received = jobs.len();
                admit(shared, jobs);
                if received == 0 {
                    let rest = idle_pause(config.poll_interval, asked.elapsed());
                    if !rest.is_zero() && shared.draining.wait_timeout(rest) {
                        return;
                    }
                }
            }
            Err(e) => {
                Counters::bump(&shared.counters.lease_errors);
                shared.counters.note_error(&e);
                let failures = match &mut outage {
                    Some((n, _)) => {
                        *n += 1;
                        *n
                    }
                    None => {
                        outage = Some((1, asked));
                        1
                    }
                };
                let delay = config.source_backoff.delay_for(failures, &mut rng);
                if failures == 1 {
                    warn!(
                        "leasing jobs failed; backing off until the source recovers";
                        error = %e,
                        retry_in = delay,
                    );
                } else {
                    debug!("leasing jobs failed again"; error = %e, attempt = failures, retry_in = delay);
                }
                if shared.draining.wait_timeout(delay) {
                    return;
                }
            }
        }
    }
}

/// Blocks until jobs can be leased; `None` once shutdown has begun.
fn wait_for_capacity(shared: &Shared) -> Option<usize> {
    let mut state = lock(&shared.state);
    loop {
        if !state.accepting {
            return None;
        }
        let n = state.lease_capacity(shared.config.concurrency, &shared.registry);
        if n > 0 {
            return Some(n.min(shared.config.max_batch()));
        }
        state = wait(&shared.changed, state);
    }
}

/// Takes ownership of freshly leased jobs.
pub(crate) fn admit(shared: &Shared, jobs: Vec<LeasedJob>) {
    for LeasedJob { job, token } in jobs {
        Counters::bump(&shared.counters.leased);
        let id = job.id;
        let Some(entry) = shared.registry.get(job.kind()) else {
            let error = unknown_kind(job.kind());
            warn!("no handler for the job's kind; failing it"; job = %id, kind = job.kind());
            shared.counters.note_error(&error);
            settle(shared, id, token, job.kind(), &Report::Fail(error));
            continue;
        };
        let tracked = Arc::new(Tracked::new(job, token, Arc::clone(entry)));
        let mut state = lock(&shared.state);
        if !state.accepting {
            // Shutdown began while the lease call was in flight.
            drop(state);
            hand_back(shared, &tracked);
            continue;
        }
        state.jobs.insert(id, Arc::clone(&tracked));
        if state.kind_has_room(&tracked.entry) {
            state.dispatch(tracked);
            drop(state);
            shared.work.notify_one();
        } else {
            debug!(
                "job waits for its kind's concurrency limit";
                job = %id,
                kind = tracked.kind(),
                limit = tracked.entry.options.concurrency,
            );
            state.parked.push_back(tracked);
        }
    }
}

/// Fails a job back to the source as "worker shutting down", due again
/// at once.
pub(crate) fn hand_back(shared: &Shared, job: &Tracked) {
    Counters::bump(&shared.counters.handed_back);
    settle(
        shared,
        job.id(),
        job.token,
        job.kind(),
        &Report::Fail(shutdown_error()),
    );
    shared.forget(job.id());
}

/// How long to pause after an empty answer that took `elapsed`: a long
/// poll that waited has paced itself; an instant empty answer (no long
/// polling, or a source that ignores it) waits out the poll interval.
fn idle_pause(poll_interval: Duration, elapsed: Duration) -> Duration {
    poll_interval.saturating_sub(elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_answers_are_paced_but_long_polls_are_not() {
        let interval = Duration::from_millis(500);
        assert_eq!(
            idle_pause(interval, Duration::from_millis(1)),
            Duration::from_millis(499)
        );
        assert_eq!(
            idle_pause(interval, Duration::from_secs(10)),
            Duration::ZERO
        );
    }
}
