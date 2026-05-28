//! Queue controls seen from workers: pausing and resuming, a token-bucket
//! rate limit driven by the manual clock, a concurrency limit enforced
//! across several workers, and purging.
//!
//! "Nothing more runs" is never checked by sleeping: the tests wait until
//! workers with free slots have asked for work and been refused a few
//! times, which proves the server held the jobs back.

use std::time::Duration;

use belay_proto::RateLimit;
use belay_testkit::prelude::*;

fn queue(name: &'static str) -> QueueName {
    QueueName::from_static(name)
}
