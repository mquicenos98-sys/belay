# belay

A durable job queue and cron scheduler in a single Rust binary.

I kept reaching for a background-job system on side projects and every
option meant running Redis or Postgres next to it, or accepting that a
restart loses whatever was queued. belay is the thing I actually wanted:
one process, its own storage, jobs that survive a crash, retries with
backoff, cron schedules that understand time zones, and an HTTP API that
any language can talk to.

No dependencies outside the standard library, on purpose. The storage
engine, the HTTP server and client, the JSON and TOML parsers, cron and
time zones, and the metrics are all in this repository, so building it
needs a Rust toolchain and nothing else.

## What it does

- **Durable jobs.** Every change is one atomic write to a write-ahead
  log with checksums and snapshots; with `sync = "always"` an
  acknowledged enqueue or completion survives a power loss, and recovery
  after a crash never leaves a hole in the history.
- **A queue with the usual controls.** Priorities, delayed and scheduled
  jobs, retries with exponential, linear or constant backoff and jitter,
  dead jobs you can retry, uniqueness keys that make enqueueing
  idempotent, per-queue concurrency and rate limits, fair sharing between
  tenants, retention, and pause and resume.
- **Leases.** Workers lease jobs over a long poll, keep them with
  heartbeats and report with a lease token, so a slow worker whose lease
  ran out cannot overwrite the result of the one that took over. Jobs of
  a worker that dies come back when its leases expire.
- **Cron schedules.** 5, 6 or 7 field expressions with `L`, `W`, `#` and
  nicknames, in any time zone, with a defined behaviour on DST changes;
  misfire and overlap policies, jitter, start and end dates, and
  calendars that keep schedules to business hours or off holidays.
- **Workflows.** DAGs of jobs with fan-out, fan-in, conditions, failure
  policies, and step payloads that read earlier steps' results.
- **An HTTP/JSON API** with bearer tokens and scopes, per-token rate
  limits, a server-sent event stream you can resume, Prometheus metrics
  and health probes.
- **A worker runtime** for Rust handlers, in the same process or over
  HTTP, and built-in `echo`, `sleep`, `fail` and `shell` handlers.
- **A command line** for all of it, plus offline tools for cron
  expressions, configuration files and data directories.
- **Clustering.** Nodes find each other by gossip and split queues and
  schedules between them; requests for another node's queue are
  redirected there.

## Quick start

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
belay dev
```

`belay dev` serves the API on `127.0.0.1:7700` with everything in
memory, and runs a worker in the same process for the `default` queue.
In another terminal:

```sh
belay enqueue default echo --payload '{"hello": "world"}' --wait
```

```text
job 01M3QM66798R5KVRD3G1M0YJ6F succeeded after 1 attempt in 0ms
{
  "hello": "world"
}
```

Workers can run anywhere that reaches the server. This one serves the
`media` queue with the shell handler, limited to one program, and runs
until it is stopped:

```sh
belay worker --queues media --allow echo
```

and from a third terminal:

```sh
belay enqueue media shell --payload '{"cmd": ["echo", "resized"]}' --wait
```

A schedule enqueues a job whenever its cron expression says so:

```sh
belay schedule add weekday-report --cron '30 9 * * MON-FRI' --tz America/Bogota \
    --queue default --kind echo --payload '{"report": "daily"}'
belay schedule preview weekday-report -n 3
belay schedule trigger weekday-report
```

And everything that happens is on the event stream:

```sh
belay events tail
belay job list
belay queue list
```

The command line is a client of the HTTP API, which you can use from any
language:

```sh
curl -d '{"queue": "emails", "kind": "send_welcome", "payload": {"user": 42}}' \
    http://127.0.0.1:7700/v1/jobs
curl -N 'http://127.0.0.1:7700/v1/events?types=job.*'
```

For a real deployment, start from
[`examples/belay.toml`](examples/belay.toml) and
[docs/operations.md](docs/operations.md):

```sh
belay config check examples/belay.toml
belay server --config examples/belay.toml
```

## Embedding

The engine is a library: a program can run belay inside itself, with
its own handlers, and serve the HTTP API too if it wants
([`crates/belay-core/examples/embedded_server.rs`](crates/belay-core/examples/embedded_server.rs)).

```rust
use std::time::Duration;

use belay_core::{Config, Engine};
use belay_json::json;
use belay_proto::JobSpec;
use belay_worker::{builtin, handler_fn, HandlerError, JobContext, Outcome, WorkerConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = Config::default(); // or Config::load(Some(path)) for a belay.toml
    config.storage.dir = "data".into();
    let engine = Engine::start(config)?;

    let registry = builtin::defaults().register(
        "send_welcome",
        handler_fn(|ctx: &mut JobContext| {
            let user = ctx.payload()["user"]
                .as_u64()
                .ok_or_else(|| HandlerError::fatal("payload has no user id"))?;
            // ... send the mail ...
            Ok(Outcome::with(json!({ "sent_to": user })))
        }),
    );
    let worker = engine.embedded_worker(
        registry,
        WorkerConfig::default().queues(vec!["emails".parse()?]),
    )?;

    let job = engine.enqueue(
        JobSpec::new("emails", "send_welcome")
            .payload(json!({ "user": 42 }))
            .max_attempts(5),
    )?;
    while !engine.get_job(&job.id)?.is_terminal() {
        std::thread::sleep(Duration::from_millis(50));
    }

    let report = worker.stop();
    println!("{}", report.stats);
    engine.shutdown();
    Ok(())
}
```

The pieces work on their own as well: `belay-queue` is the queue engine
over any store
([`crates/belay-queue/examples/embedded.rs`](crates/belay-queue/examples/embedded.rs)),
`belay-client` a typed client for a remote server
([`crates/belay-client/examples/enqueue.rs`](crates/belay-client/examples/enqueue.rs)),
and `belay-worker` runs your handlers against either
([`crates/belay-worker/examples/worker.rs`](crates/belay-worker/examples/worker.rs)).

## Documentation

| document | covers |
|---|---|
| [architecture.md](docs/architecture.md) | the crates, a request's path to the disk, threads, design decisions |
| [configuration.md](docs/configuration.md) | every configuration key, environment overrides, validation |
| [http-api.md](docs/http-api.md) | every endpoint, errors, pagination, scopes, redirects, events |
| [cli.md](docs/cli.md) | the `belay` command |
| [operations.md](docs/operations.md) | installing, systemd, sync policies, backups, upgrades, monitoring, capacity, troubleshooting |
| [storage.md](docs/storage.md) | the log-structured store and its recovery |
| [queue.md](docs/queue.md) | the job lifecycle, leases, retries, uniqueness, limits, fair sharing |
| [workers.md](docs/workers.md) | the worker runtime and the built-in handlers |
| [cron.md](docs/cron.md) | cron expressions and time zones |
| [scheduling.md](docs/scheduling.md) | schedules, misfires, overlap, jitter, calendars |
| [workflows.md](docs/workflows.md) | workflow DAGs |
| [cluster.md](docs/cluster.md) | membership, ownership and redirects |
| [metrics.md](docs/metrics.md) | the metric families |

[`examples/`](examples/) has configuration files for a single node and a
three-node cluster, schedules, a calendar, a workflow and a systemd unit.
Every crate's documentation (`cargo doc --workspace --no-deps --open`)
goes into more detail. Changes are listed in [CHANGELOG.md](CHANGELOG.md).

## Status

0.6 is solid for what it sets out to do: I run it for my own projects,
the storage engine is tested at every crash point of its log, and an
end-to-end suite drives the whole system through the HTTP API. It is
still 0.x, so a minor version may change behaviour; the changelog says
when it does.

- [x] shared foundations, JSON, TOML configuration
- [x] a log-structured store with crash recovery
- [x] the queue engine and the worker runtime
- [x] cron expressions with time zones; schedules, calendars, workflows
- [x] the HTTP API, a Rust client, metrics and the event stream
- [x] clustering by gossip and ownership
- [x] the `belay` binary
- [x] an end-to-end test suite and benchmarks
- [x] documentation

Limitations, on purpose or for now:

- **Plain HTTP.** There is no TLS; put a TLS-terminating proxy in front
  of servers that clients reach over an untrusted network. Gossip between
  cluster nodes is neither authenticated nor encrypted either.
- **The data lives in memory.** The store holds the whole dataset in
  memory (twice during a checkpoint), which suits job records of a few
  hundred bytes; keep large payloads elsewhere and pass references.
- **At-least-once.** A job can run more than once (a lost lease, a
  crash), so handlers must be idempotent.
- **No replication.** A cluster spreads queues over nodes but does not
  copy them: a node's jobs are unavailable while it is down.
- **One writer.** The queue engine writes under one lock, so with
  `sync = "always"` its throughput is bounded by the disk's sync latency;
  batch enqueues and multi-job leases help.
- **Current time zone rules only.** Each zone uses its current DST rule,
  also for past dates.
- **No signal handling.** The standard library cannot catch signals, so
  Ctrl-C ends a server at once (safely); `belay admin shutdown` drains it.
- **Events are not persisted.** The event stream keeps a buffer in
  memory; a restart starts a new one.

## Building and testing

```sh
cargo build --release -p belay-cli    # target/release/belay
cargo test --workspace                # unit, integration and end-to-end tests
cargo run --release -p belay-bench -- all --quick
```

Rust 1.80 or newer, on Linux, macOS or Windows. [CONTRIBUTING.md](CONTRIBUTING.md)
has the details: the crate layout, the conventions, the end-to-end suite
and the benchmarks.

## License

MIT OR Apache-2.0, at your option.
