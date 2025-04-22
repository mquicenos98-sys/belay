//! Token bucket behind a queue's `rate_limit`.
//!
//! The bucket holds up to `burst` tokens and refills continuously at
//! `per_second`; each lease grant takes one. Time comes from the engine's
//! clock and is passed in, so a manual clock drives it exactly in tests. A
//! clock that steps backwards refills nothing until it passes the last
//! refill time again, rather than minting tokens from negative time.

use std::time::Duration;

use belay_common::Timestamp;
use belay_proto::RateLimit;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TokenBucket {
    limit: RateLimit,
    tokens: f64,
    updated: Timestamp,
}

impl TokenBucket {
    /// A full bucket: a queue that was idle may burst right away.
    pub(crate) fn new(limit: RateLimit, now: Timestamp) -> TokenBucket {
        TokenBucket {
            limit,
            tokens: f64::from(limit.burst),
            updated: now,
        }
    }

    fn refill(&mut self, now: Timestamp) {
        if let Some(elapsed) = now.duration_since(self.updated) {
            let added = elapsed.as_secs_f64() * self.limit.per_second;
            self.tokens = (self.tokens + added).min(f64::from(self.limit.burst));
            self.updated = now;
        }
    }

    /// Takes a token if one is available at `now`.
    pub(crate) fn try_take(&mut self, now: Timestamp) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Tokens available at `now`, fractional.
    pub(crate) fn available(&self, now: Timestamp) -> f64 {
        let mut copy = *self;
        copy.refill(now);
        copy.tokens
    }

    /// How long until a token is available, zero if one is now.
    pub(crate) fn wait_time(&self, now: Timestamp) -> Duration {
        let missing = 1.0 - self.available(now);
        if missing <= 0.0 {
            Duration::ZERO
        } else {
            // Round up to the next millisecond: waking a hair early would
            // find no token and go back to sleep.
            let secs = missing / self.limit.per_second;
            let millis = (secs * 1000.0).ceil();
            Duration::from_millis(if millis.is_finite() {
                millis.min(u64::MAX as f64) as u64
            } else {
                u64::MAX
            })
        }
    }

    /// Switches to a new limit, keeping the tokens accumulated so far (up
    /// to the new burst) so that reconfiguring cannot be used to refill.
    pub(crate) fn reconfigure(&mut self, limit: RateLimit, now: Timestamp) {
        self.refill(now);
        self.limit = limit;
        self.tokens = self.tokens.min(f64::from(limit.burst));
    }

    #[cfg(test)]
    pub(crate) fn limit(&self) -> RateLimit {
        self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: i64) -> Timestamp {
        Timestamp::from_millis(1_000_000 + ms)
    }

    #[test]
    fn starts_full_and_refills_at_the_configured_rate() {
        let mut b = TokenBucket::new(RateLimit::new(2.0, 3), at(0));
        assert!(b.try_take(at(0)) && b.try_take(at(0)) && b.try_take(at(0)));
        assert!(!b.try_take(at(0)));
        assert_eq!(b.wait_time(at(0)), Duration::from_millis(500));
        assert!(!b.try_take(at(499)));
        assert!(b.try_take(at(500)));
        assert!(!b.try_take(at(500)));
        // Idle for a long time: capped at the burst.
        assert!((b.available(at(60_000)) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_clock_stepping_back_mints_nothing() {
        let mut b = TokenBucket::new(RateLimit::new(1.0, 1), at(10_000));
        assert!(b.try_take(at(10_000)));
        assert!(!b.try_take(at(0)));
        assert!(!b.try_take(at(10_500)));
        assert!(b.try_take(at(11_000)));
    }

    #[test]
    fn reconfiguring_keeps_accumulated_tokens_up_to_the_new_burst() {
        let mut b = TokenBucket::new(RateLimit::new(1.0, 10), at(0));
        for _ in 0..8 {
            assert!(b.try_take(at(0)));
        }
        b.reconfigure(RateLimit::new(100.0, 50), at(0));
        assert!((b.available(at(0)) - 2.0).abs() < 1e-9);
        assert_eq!(b.limit().burst, 50);
        b.reconfigure(RateLimit::new(1.0, 1), at(1_000));
        assert!((b.available(at(1_000)) - 1.0).abs() < 1e-9);
    }
}
