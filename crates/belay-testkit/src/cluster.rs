//! [`TestCluster`] and its builder: harness servers gossiping over one
//! in-memory network, with membership and ownership helpers.

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use belay_cluster::{MemNetwork, Transport};
use belay_common::Timestamp;
use belay_core::config::ClusterProfile;
use belay_core::{Config, Engine, Ownership, TickReport};
use belay_proto::api::MemberState;
use belay_proto::{QueueName, ScheduleId};

use crate::error::{Error, Result};
use crate::fixtures::{queue_name, start_time};
use crate::logs::init_test_logging;
use crate::server::{ClusterSeat, TestClock, TestServer};
use crate::wait::eventually;

type Configure = Box<dyn Fn(usize, &mut Config)>;

/// Builds a [`TestCluster`].
#[must_use]
pub struct TestClusterBuilder {
    size: usize,
    clock: TestClock,
    seed: u64,
    configure: Vec<Configure>,
}
