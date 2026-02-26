//! Three engines on an in-memory network: they agree on who owns each
//! queue and schedule, and each schedule fires on exactly one of them,
//! also after a node leaves.

mod common;

use std::sync::Arc;
use std::time::Duration;

use belay_cluster::MemNetwork;
use belay_common::{Clock, ManualClock, Timestamp};
use belay_core::config::ClusterProfile;
use belay_core::{Config, Engine, Ownership};
use belay_proto::api::MemberState;
use belay_proto::{QueueName, ScheduleId};
use common::{eventually, quiet_logs, START};
