//! Two nodes gossiping over an in-memory network, each with a real HTTP
//! server: requests for queues and schedules owned by the other node are
//! redirected there, and the typed client follows.

mod common;

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use belay_cluster::{MemNetwork, Transport};
use belay_common::ManualClock;
use belay_core::config::ClusterProfile;
use belay_core::{Engine, Ownership};
use belay_http::Method;
use belay_proto::api::{ErrorCode, MemberState};
use belay_proto::{JobSpec, QueueName, ScheduleId};
use belay_server::{ServerHandle, ServerOptions, REMOTE_QUEUES_HEADER};
use common::{api_error, eventually, json, quiet_logs, test_config, START};

struct Node {
    engine: Arc<Engine>,
    handle: ServerHandle,
    url: String,
}
