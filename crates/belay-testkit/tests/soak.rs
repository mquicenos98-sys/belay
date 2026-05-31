//! A short, bounded soak in real time: 2,000 jobs across four queues,
//! three workers with four slots each, and handlers that fail at random.
//! Every job must end succeeded or dead exactly once (counted on the event
//! stream), the one predicted by the handlers' deterministic failures, and
//! no job may ever be leased twice at once.

use std::collections::{BTreeMap, BTreeSet};
use std::thread;
use std::time::Duration;

use belay_common::BackoffPolicy;
use belay_testkit::prelude::*;

const QUEUES: [&str; 4] = ["soak-a", "soak-b", "soak-c", "soak-d"];

const PER_QUEUE: usize = 500;

const BATCH: usize = 100;
