//! Three nodes on an in-memory gossip network, each with its own HTTP
//! server, store and worker: requests are redirected to the owning node,
//! each queue is processed where it is owned, schedules fire once across
//! the cluster, and a crashed node's queues move to the survivors and come
//! back when it returns.

use std::collections::BTreeMap;
use std::time::Duration;

use belay_proto::api::MemberState;
use belay_proto::job::META_SCHEDULE;
use belay_testkit::prelude::*;
use belay_testkit::raw;

const MEMBERSHIP_TIMEOUT: Duration = Duration::from_secs(30);
