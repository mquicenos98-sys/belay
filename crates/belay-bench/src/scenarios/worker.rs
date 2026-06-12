//! `worker-e2e`: jobs through the worker runtime with handlers that do
//! nothing, so the numbers are the runtime's own cost.
//!
//! - `mode=drain`: a backlog is enqueued while the queue is paused, then
//!   the queue is resumed and the clock runs until the worker has
//!   completed every job. Throughput only; per-job latency in a drained
//!   backlog is mostly time spent waiting behind other jobs.
//! - `mode=pickup`: one job at a time is enqueued into an idle worker's
//!   queue; the latency is from `enqueue` until the handler starts, i.e.
//!   the long-poll wake-up, the lease and the dispatch to an executor.
//!
//! `source=embedded` leases in-process from the engine (as `belay dev`
//! does), `source=http` through `belay-client` and `belay-server` over
//! loopback.

use std::fmt;
use std::net::TcpListener;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use belay_client::{Client, ClientConfig};
use belay_core::{Config, Engine};
use belay_proto::{JobId, JobSpec, QueueName, WorkerId};
use belay_server::ServerHandle;
use belay_worker::{handler_fn, Outcome, Registry, Worker, WorkerConfig, WorkerHandle};

use crate::harness::{measure, Params, Plan, Sample, Work};
use crate::session::{CaseResult, Session};
use crate::{Error, Result};

pub const NAME: &str = "worker-e2e";

/// How long a lease call waits for work. Short, because stopping a worker
/// waits for its in-flight lease call; an enqueue wakes the call at once
/// anyway.
const POLL_WAIT: Duration = Duration::from_millis(100);

/// Longest a drain may take before the case is declared failed.
const DRAIN_DEADLINE: Duration = Duration::from_secs(300);

/// Longest a single job may wait to be picked up.
const PICKUP_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Embedded,
    Http,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Embedded => "embedded",
            Source::Http => "http",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Drain,
    Pickup,
}

struct Case {
    source: Source,
    mode: Mode,
    concurrency: usize,
    params: Params,
}

pub fn run(s: &mut Session<'_>) -> Result<()> {
    let concurrency: Vec<usize> = s.thread_counts().into_iter().map(|t| t * 4).collect();
    let mut cases = Vec::new();
    for source in [Source::Embedded, Source::Http] {
        for &slots in &concurrency {
            cases.push(case(source, Mode::Drain, slots));
        }
        cases.push(case(source, Mode::Pickup, 4));
    }
    cases.retain(|c| s.wants(NAME, &c.params));
    if cases.is_empty() {
        return Ok(());
    }

    let engine = Arc::new(
        Engine::builder()
            .config(Config::dev())
            .seed(s.seed())
            .start()?,
    );
    let server = if cases.iter().any(|c| c.source == Source::Http) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| Error::io("binding a loopback port", e))?;
        Some(belay_server::serve_on(Arc::clone(&engine), listener)?)
    } else {
        None
    };
    let outcome = run_cases(s, &engine, server.as_ref(), cases);
    match server {
        Some(server) => server.shutdown_and_join(),
        None => engine.shutdown(),
    }
    outcome
}

fn case(source: Source, mode: Mode, concurrency: usize) -> Case {
    let mode_name = match mode {
        Mode::Drain => "drain",
        Mode::Pickup => "pickup",
    };
    Case {
        source,
        mode,
        concurrency,
        params: Params::new()
            .with("source", source)
            .with("mode", mode_name)
            .with("concurrency", concurrency),
    }
}

fn run_cases(
    s: &mut Session<'_>,
    engine: &Engine,
    server: Option<&ServerHandle>,
    cases: Vec<Case>,
) -> Result<()> {
    for (index, case) in cases.into_iter().enumerate() {
        let queue = QueueName::new(format!("bench-worker-{index}"))
            .expect("bench-worker-<n> is a valid queue name");
        let worker = WorkerId::new(format!("bench-{}-{index}", case.source))
            .expect("bench-<source>-<n> is a valid worker id");
        let config = WorkerConfig::new(worker, vec![queue.clone()])
            .concurrency(case.concurrency)
            .lease_for(Duration::from_secs(300))
            .poll_wait(POLL_WAIT)
            .poll_interval(Duration::from_millis(1));
        let start = |registry: Registry| -> Result<WorkerHandle> {
            match (case.source, server) {
                (Source::Http, Some(server)) => {
                    let url = format!("http://{}", server.local_addr());
                    let client =
                        Client::new(ClientConfig::new(url).timeout(Duration::from_secs(30)))?;
                    Ok(Worker::new(Arc::new(client), registry, config.clone())?.spawn()?)
                }
                _ => Ok(engine.embedded_worker(registry, config.clone())?),
            }
        };
        let result = match case.mode {
            Mode::Drain => {
                let jobs = s.pick(50_000, 10_000, 20);
                let warmup = s.pick(2_000, 500, 2);
                CaseResult::new(
                    NAME,
                    case.params,
                    drain(engine, &queue, &start, warmup, jobs)?,
                )
            }
            Mode::Pickup => {
                let plan = Plan::new(s.pick(2_000, 300, 3));
                CaseResult::new(NAME, case.params, pickup(engine, &queue, &start, plan)?)
            }
        };
        s.record(result);
    }
    Ok(())
}
