//! Loads a realistic belay configuration the way the engine does: parse,
//! overlay the environment, extract typed values through a `Reader`, then
//! warn about keys nobody read.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use belay_conf::{Document, Error, Origin, Overlay, Reader, Result, Value};

const BELAY_TOML: &str = r#"# belay.toml - production node eu-1
[server]
listen = "0.0.0.0:7700"
threads = 16
max_body = 8_388_608   # 8 MiB

[storage]
dir = '/var/lib/belay'
in_memory = false
sync = "interval:10ms"
checkpoint_bytes = 0x4000_0000

[auth]
tokens = [
  { name = "ops", token_sha256 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08", scopes = ["admin"] },  # gitleaks:allow
  { name = "ci", token = "ci-secret", scopes = ["enqueue", "read"] },
]

[log]
level = "info"
format = "logfmt"

[queues.emails]
max_concurrency = 8
rate_limit = { per_second = 50.0, burst = 100 }
default_timeout = "30s"

[queues."reports.daily"]
max_concurrency = 1
default_retry = "exp:1s*2..10m"

[[schedules]]
id = "nightly-report"
cron = "0 3 * * *"
timezone = "Europe/Berlin"
starts_at = 2025-01-01T00:00:00Z
template = { queue = "reports.daily", kind = "report.build", payload = { format = "pdf" } }

[[schedules]]
id = "heartbeat"
cron = "*/5 * * * *"
misfire = "skip"
jitter = "10s"

  [schedules.template]
  queue = "emails"
  kind = "ping"
  meta = { team = "platform" }

[scheduler]
tick = "250ms"

[retention]
succeeded = "7d"
dead = "30d"

[cluster]
node_id = "eu-1"
gossip_listen = "0.0.0.0:7946"
seeds = ["10.0.0.2:7946", "10.0.0.3:7946"]

[metrics]
enabled = true
"#;

#[derive(Debug, PartialEq)]
struct Token {
    name: String,
    secret: String,
    scopes: Vec<String>,
}

#[derive(Debug, PartialEq)]
struct Queue {
    name: String,
    max_concurrency: Option<u32>,
    rate_limit: Option<(f64, u32)>,
    default_timeout: Option<String>,
}

#[derive(Debug, PartialEq)]
struct Schedule {
    id: String,
    cron: String,
    timezone: String,
    starts_at: Option<String>,
    queue: String,
    kind: String,
    meta: BTreeMap<String, String>,
}

#[derive(Debug)]
struct Config {
    listen: SocketAddr,
    threads: u32,
    max_body: u64,
    dir: String,
    sync: String,
    checkpoint_bytes: u64,
    tokens: Vec<Token>,
    log_level: String,
    queues: Vec<Queue>,
    schedules: Vec<Schedule>,
    seeds: Vec<String>,
    metrics: bool,
}
