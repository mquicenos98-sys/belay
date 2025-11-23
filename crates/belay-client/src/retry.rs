//! Which failed requests the client repeats on its own, and how long it
//! waits in between.
//!
//! A request is repeated only when repeating it cannot do anything the
//! first attempt did not already do. Every call falls into one of three
//! classes:
//!
//! | class | calls |
//! |---|---|
//! | **repeatable** | every `GET`, `PUT` and `DELETE`; job `cancel`; queue and schedule `pause`/`resume`; workflow `cancel`; admin `checkpoint`; the worker calls `heartbeat`, `complete` and `fail`; `enqueue` when [every spec has a key that deduplicates repeats](is_repeatable_enqueue) |
//! | **not repeatable** | other `enqueue`s, `lease`, job `retry` and `promote`, queue `purge`, schedule `create` and `trigger`, workflow `submit` |
//! | **never retried** | `health` and `ready`: a probe's first answer is the point |
//!
//! The worker calls are repeatable because the lease token makes them
//! at-most-once: after a `complete` that the server applied but whose
//! answer was lost, the repeat fails with `lease_lost` instead of acting
//! twice, and a worker treats `lease_lost` as "drop the job", which is
//! harmless for a job that already finished. `lease` is not repeatable: a
//! lost answer would leave the first batch leased to nobody until the
//! lease expires. `retry` and `promote` can act twice if the job moved on
//! between the attempts.
//!
//! What triggers a repeat:
//!
//! | failure | repeatable | not repeatable |
//! |---|---|---|
//! | connection refused, connect timeout | yes | **yes**: nothing was sent |
//! | connection closed or reset mid-exchange | yes | no |
//! | `502`, `503`, `504`, `429` | yes | no |
//! | read or write timeout | no | no |
//! | any other status | no | no |
//!
//! Timeouts are not repeated because [`timeout`](crate::ClientConfig::timeout)
//! is the caller's bound on how long to wait for a hung server, and
//! repeating would multiply it.
//!
//! The wait before retry `n` is [`backoff`](crate::ClientConfig::backoff)`.delay_for(n)`,
//! lengthened to the server's `Retry-After` (seconds or an HTTP date) when
//! it asks for more. A `Retry-After` longer than the client's `timeout` is
//! not waited out: the error is returned at once so the caller can decide.

use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use belay_common::sync::lock;
use belay_common::{BackoffPolicy, Rng};
use belay_proto::{JobSpec, OnConflict, UniqueScope};

use crate::error::never_sent;

/// Whether an enqueue of `spec` can be sent twice without creating two
/// jobs: it carries a uniqueness key that answers a repeat with the job
/// the first attempt created (`on_conflict: return_existing`), held at
/// least until that job finishes or for a fixed period.
///
/// `until_started` keys do not qualify, since a worker may start the job
/// between the two attempts and free the key.
///
/// ```
/// use std::time::Duration;
/// use belay_client::retry::is_repeatable_enqueue;
/// use belay_proto::{JobSpec, OnConflict, Unique, UniqueScope};
///
/// assert!(!is_repeatable_enqueue(&JobSpec::new("emails", "send")));
/// assert!(is_repeatable_enqueue(
///     &JobSpec::new("emails", "send").unique("user:42", UniqueScope::UntilFinished)
/// ));
/// assert!(!is_repeatable_enqueue(&JobSpec::new("emails", "send").unique_with(
///     Unique::new("user:42", UniqueScope::For(Duration::from_secs(60)))
///         .on_conflict(OnConflict::Reject)
/// )));
/// ```
#[must_use]
pub fn is_repeatable_enqueue(spec: &JobSpec) -> bool {
    spec.unique.as_ref().is_some_and(|u| {
        u.on_conflict == OnConflict::ReturnExisting
            && matches!(u.scope, UniqueScope::UntilFinished | UniqueScope::For(_))
    })
}

/// How safe a request is to repeat; see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Safety {
    /// Repeating has no effect beyond the first success.
    Repeatable,
    /// Repeating may act twice; only resent when provably never sent.
    NotRepeatable,
    /// Never resent.
    Never,
}

/// Why an attempt failed, as far as retrying is concerned.
#[derive(Debug)]
pub(crate) enum Failure<'a> {
    /// No answer.
    Transport(&'a belay_http::Error),
    /// An error status, with the server's `Retry-After` if it sent one.
    Status {
        status: u16,
        retry_after: Option<Duration>,
    },
}

/// Statuses that mean "try again later" without having acted.
fn is_retry_status(status: u16) -> bool {
    matches!(status, 429 | 502..=504)
}

/// The retry budget and pacing of one client.
#[derive(Debug)]
pub(crate) struct RetryPolicy {
    retries: u32,
    backoff: BackoffPolicy,
    /// The longest `Retry-After` worth waiting for.
    max_wait: Duration,
    rng: Mutex<Rng>,
}

impl RetryPolicy {
    pub(crate) fn new(retries: u32, backoff: BackoffPolicy, max_wait: Duration) -> RetryPolicy {
        RetryPolicy {
            retries,
            backoff,
            max_wait,
            rng: Mutex::new(Rng::from_entropy()),
        }
    }

    /// The wait before the next attempt after attempt number `attempt`
    /// (1 = the first) failed with `failure`, or `None` to give up.
    pub(crate) fn next_delay(
        &self,
        attempt: u32,
        safety: Safety,
        failure: &Failure<'_>,
    ) -> Option<Duration> {
        if attempt > self.retries || safety == Safety::Never {
            return None;
        }
        let repeatable = safety == Safety::Repeatable;
        let retry_after = match failure {
            Failure::Transport(e) if never_sent(e) => None,
            Failure::Transport(e) if repeatable && e.is_transient() && !e.is_timeout() => None,
            Failure::Status {
                status,
                retry_after,
            } if repeatable && is_retry_status(*status) => *retry_after,
            _ => return None,
        };
        let backoff = self.backoff.delay_for(attempt, &mut lock(&self.rng));
        match retry_after {
            Some(wait) if wait > self.max_wait => None,
            Some(wait) => Some(wait.min(backoff)),
            None => Some(backoff),
        }
    }
}
