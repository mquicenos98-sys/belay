//! Cron schedules over the HTTP API, walked with a manual clock: a named
//! zone across both daylight-saving transitions, each misfire policy after
//! the server was down for hours, and pausing and triggering by hand.

use std::time::Duration;

use belay_common::Timestamp;
use belay_proto::job::{META_FIRE_TIME, META_SCHEDULE};
use belay_proto::Retention;
use belay_testkit::fixtures::SCHEDULED_QUEUE;
use belay_testkit::prelude::*;

fn at(text: &str) -> Timestamp {
    Timestamp::parse_rfc3339(text).expect("test timestamps are valid RFC 3339")
}

fn fire_time(job: &Job) -> Option<Timestamp> {
    job.spec
        .meta
        .get(META_FIRE_TIME)
        .and_then(|t| Timestamp::parse_rfc3339(t).ok())
}
