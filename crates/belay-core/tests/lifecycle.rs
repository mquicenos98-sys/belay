//! Starting, stopping and restarting engines: durability on disk,
//! recovery of workflows, checkpoints, shutdown, and the background
//! threads in real time.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use belay_common::fs::TempDir;
use belay_common::{Clock, ManualClock, Timestamp};
use belay_core::{Config, Engine, Error, Ownership, Paging};
use belay_json::json;
use belay_proto::api::{CompleteRequest, ErrorCode, LeaseRequest};
use belay_proto::event::EventFilter;
use belay_proto::workflow::{StepSpec, StepState, WorkflowState};
use belay_proto::{JobSource, JobSpec, JobState, QueueName, ScheduleSpec, WorkerId, WorkflowSpec};
use belay_queue::{EngineOptions, QueueEngine};
use belay_store::{LogKv, Options};
use belay_worker::{builtin, WorkerConfig};
use common::{eventually, quiet_logs, START};

const CONFIG: &str = r#"
[queues.emails]
max_concurrency = 3

[[schedules]]
id = "nightly"
cron = "0 3 * * *"
template = { queue = "reports", kind = "nightly" }
"#;

fn disk_config(dir: &Path) -> Config {
    let mut config = Config::from_str(CONFIG).unwrap();
    config.storage.dir = dir.to_path_buf();
    config
}
