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

fn main() -> Result<(), Box<dyn Error>> {
    let dir = TempDir::with_prefix("belay-embedded-server")?;
    let mut config = Config::from_str(CONFIG)?;
    config.storage.dir = dir.path().to_path_buf();
    for warning in config.warnings() {
        eprintln!("{warning}");
    }

    // The engine opens the store, applies the configured queue and
    // schedule, and starts its background threads.
    let engine = Arc::new(Engine::start(config)?);

    // Port 0: the operating system picks a free port.
    let server = belay_server::serve_on(Arc::clone(&engine), TcpListener::bind("127.0.0.1:0")?)?;
    let url = format!("http://{}", server.local_addr());
    println!("serving the belay API on {url}");

    // A worker in this process leases straight from the engine, without
    // HTTP. Workers elsewhere would use belay-client as their source.
    let registry = builtin::defaults().register("build_report", handler_fn(build_report));
    let worker = engine.embedded_worker(
        registry,
        WorkerConfig::default()
            .queues(vec!["reports".parse()?, "default".parse()?])
            .poll_wait(Duration::from_secs(1)),
    )?;

    // Anyone else talks HTTP; here, a client in the same process.
    let client = Client::new(ClientConfig::new(url.as_str()))?;
    let schedule = client.schedules().get(&"hourly-report".parse()?)?;
    if let Some(next) = schedule.next_fire_at {
        println!("schedule hourly-report next fires at {next}");
    }
    let created = client
        .jobs()
        .enqueue(&JobSpec::new("reports", "build_report").payload(json!({"scope": "day"})))?;
    println!("enqueued {} over HTTP", created.id);

    let deadline = Instant::now() + Duration::from_secs(10);
    let job = loop {
        let job = client.jobs().get(created.id)?;
        if job.is_terminal() || Instant::now() >= deadline {
            break job;
        }
        thread::sleep(Duration::from_millis(20));
    };
    println!(
        "job {} {} with result {}",
        job.id,
        job.state,
        job.result
            .as_ref()
            .map_or_else(|| "none".to_owned(), ToString::to_string)
    );

    let queue = engine.queue_info(&"reports".parse()?)?;
    println!(
        "queue reports: {} succeeded, max_concurrency {:?}",
        queue.counts.succeeded, queue.config.max_concurrency
    );

    // Stop the worker first, while the engine still takes its results;
    // then drain the server, which shuts the engine down.
    let report = worker.stop();
    println!("worker stopped: {}", report.stats);
    server.shutdown_and_join();
    assert_eq!(job.state, JobState::Succeeded);
    Ok(())
}
