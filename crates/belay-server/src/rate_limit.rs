//! Request rate limiting per bearer token.
//!
//! Each token gets a bucket that holds up to `burst` requests and refills
//! continuously at `per_second`; a request takes one. An empty bucket
//! answers how long until the next request would fit, which becomes the
//! `Retry-After` of the `429`. Buckets are created on a token's first
//! request and kept for the life of the server: they are keyed by token
//! *name*, and only configured tokens get past authentication, so the map
//! cannot grow beyond the configuration.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use belay_common::sync::lock;

use crate::options::{RequestRate, ServerOptions};

/// One token's bucket.
#[derive(Clone, Debug)]
struct Bucket {
    rate: RequestRate,
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    /// A full bucket: a token that was idle may burst right away.
    fn full(rate: RequestRate, now: Instant) -> Bucket {
        Bucket {
            rate,
            tokens: f64::from(rate.burst),
            updated: now,
        }
    }

    /// Takes one request's worth, or says how long until there is one.
    fn take(&mut self, now: Instant) -> Result<(), Duration> {
        // Instants handed in by concurrent requests may arrive slightly
        // out of order; an earlier one simply refills nothing.
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        let capacity = f64::from(self.rate.burst);
        self.tokens = (self.tokens + elapsed * self.rate.per_second).min(capacity);
        self.updated = self.updated.max(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            let missing = 1.0 - self.tokens;
            Err(Duration::from_secs_f64(missing / self.rate.per_second))
        }
    }
}

/// Token buckets for every limited token.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    options: ServerOptions,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub(crate) fn new(options: &ServerOptions) -> RateLimiter {
        RateLimiter {
            options: options.clone(),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Admits one request from `token` at `now`, or returns how long it
    /// should wait.
    pub(crate) fn check(&self, token: &str, now: Instant) -> Result<(), Duration> {
        let Some(rate) = self.options.rate_for(token) else {
            return Ok(());
        };
        let mut buckets = lock(&self.buckets);
        match buckets.get_mut(token) {
            Some(bucket) => bucket.take(now),
            None => {
                let mut bucket = Bucket::full(rate, now);
                let taken = bucket.take(now);
                buckets.insert(token.to_owned(), bucket);
                taken
            }
        }
    }
}

/// A `Retry-After` value in whole seconds (RFC 9110 §10.2.3), at least 1.
#[must_use]
pub(crate) fn retry_after_secs(wait: Duration) -> u64 {
    (wait.as_secs_f64().round() as u64).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(rate: Option<RequestRate>) -> RateLimiter {
        RateLimiter::new(&ServerOptions::new().with_rate_limit(rate))
    }

    #[test]
    fn a_fresh_token_may_burst_then_waits_for_refill() {
        let limiter = limiter(Some(RequestRate::new(2.0, 3)));
        let t0 = Instant::now();
        for _ in 0..3 {
            assert_eq!(limiter.check("ci", t0), Ok(()));
        }
        let wait = limiter.check("ci", t0).unwrap_err();
        assert_eq!(wait, Duration::from_millis(500));
        // Half a second later exactly one request fits again.
        let t1 = t0 + Duration::from_millis(500);
        assert_eq!(limiter.check("ci", t1), Ok(()));
        assert!(limiter.check("ci", t1).is_err());
    }

    #[test]
    fn tokens_have_separate_buckets() {
        let limiter = limiter(Some(RequestRate::new(1.0, 1)));
        let now = Instant::now();
        assert_eq!(limiter.check("a", now), Ok(()));
        assert!(limiter.check("a", now).is_err());
        assert_eq!(limiter.check("b", now), Ok(()));
    }

    #[test]
    fn refill_never_exceeds_the_burst() {
        let limiter = limiter(Some(RequestRate::new(100.0, 2)));
        let t0 = Instant::now();
        assert_eq!(limiter.check("a", t0), Ok(()));
        let later = t0 + Duration::from_secs(3600);
        assert_eq!(limiter.check("a", later), Ok(()));
        assert_eq!(limiter.check("a", later), Ok(()));
        assert!(limiter.check("a", later).is_err());
    }

    #[test]
    fn unlimited_tokens_are_never_refused() {
        let options = ServerOptions::new()
            .with_rate_limit(Some(RequestRate::new(1.0, 1)))
            .with_token_rate_limit("ops", None);
        let ops = RateLimiter::new(&options);
        let now = Instant::now();
        for _ in 0..100 {
            assert_eq!(ops.check("ops", now), Ok(()));
        }
        assert!(limiter(None).check("anyone", now).is_ok());
    }

    // Regression: an Instant older than the bucket's last update (two
    // requests racing for the lock) used to panic in the subtraction.
    #[test]
    fn out_of_order_instants_refill_nothing() {
        let limiter = limiter(Some(RequestRate::new(1.0, 1)));
        let later = Instant::now() + Duration::from_secs(10);
        assert_eq!(limiter.check("a", later), Ok(()));
        let earlier = later - Duration::from_secs(5);
        assert!(limiter.check("a", earlier).is_err());
    }
}
