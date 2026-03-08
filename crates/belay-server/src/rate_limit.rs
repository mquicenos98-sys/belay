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
