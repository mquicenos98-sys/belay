//! A belay server inside your own program: an [`Engine`] configured from
//! TOML, the HTTP API served on a local port with belay-server, a worker
//! running in the same process, and a client talking to it over HTTP.
//!
//! ```text
//! cargo run -p belay-core --example embedded_server
//! ```
//!
//! Useful when an application wants a job queue without running a
//! separate service, while still letting other processes (workers in
//! other languages, `belay` on the command line) reach it over HTTP.

use std::error::Error;
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use belay_client::{Client, ClientConfig};
use belay_common::fs::TempDir;
use belay_core::{Config, Engine};
use belay_json::json;
use belay_proto::{JobSpec, JobState};
use belay_worker::{builtin, handler_fn, HandlerError, JobContext, Outcome, WorkerConfig};

/// The configuration, as it would be in a belay.toml. The data directory
/// is set below, to a temporary one.
const CONFIG: &str = r#"
[storage]
sync = "interval:10ms"

[queues.reports]
max_concurrency = 2
default_timeout = "5m"
default_max_attempts = 3

[[schedules]]
id = "hourly-report"
cron = "0 * * * *"
template = { queue = "reports", kind = "build_report", payload = { scope = "hour" } }
"#;

fn build_report(ctx: &mut JobContext) -> Result<Outcome, HandlerError> {
    let scope = ctx.payload()["scope"].as_str().unwrap_or("day").to_owned();
    ctx.progress(50.0, format!("summing the last {scope}"));
    Ok(Outcome::with(json!({"scope": scope, "orders": 128})))
}
