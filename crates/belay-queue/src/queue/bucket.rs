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
