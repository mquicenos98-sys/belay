# belay

A durable job queue and cron scheduler in a single Rust binary.

I keep reaching for a background-job system on side projects and every
option means running Redis or Postgres next to it, or accepting that a
restart loses whatever was queued. belay is an attempt at the thing I
actually want: one process, its own storage, jobs that survive a crash,
retries with backoff, cron schedules that understand time zones, and an
HTTP API that any language can talk to.

No dependencies outside the standard library, on purpose. That includes
the HTTP server and client, the JSON and TOML parsers, and the metrics.

## Status

0.3.0 is the first release with a server: `belay` is now a binary that
serves the queue, the scheduler and workflows over HTTP, runs workers,
and manages all of it from the command line. The libraries of 0.1 and
0.2 are its engine and can still be embedded on their own.

- [x] shared foundations (`belay-common`), JSON (`belay-json`), config
      (`belay-conf`)
- [x] job and queue model (`belay-proto`)
- [x] a log-structured store with crash recovery (`belay-store`)
- [x] the queue engine (`belay-queue`) and worker runtime (`belay-worker`)
- [x] cron expressions with time zones and DST handling (`belay-cron`)
- [x] schedules, calendars and workflows (`belay-scheduler`)
- [x] HTTP on `std::net` (`belay-http`) and Prometheus metrics
      (`belay-metrics`)
- [x] the engine and its configuration file (`belay-core`)
- [x] the HTTP/JSON API with tokens, scopes and an event stream
      (`belay-server`), and a Rust client for it (`belay-client`)
- [x] clustering: gossip membership and queue ownership
      (`belay-cluster`)
- [x] the `belay` binary (`belay-cli`)
- [ ] an end-to-end test suite that drives everything over HTTP
- [ ] benchmarks
- [ ] documentation beyond `--help` and the crate docs

## Trying it

```
cargo build --release
./target/release/belay dev
```

`belay dev` serves the API on `127.0.0.1:7700` with everything in
memory and runs a worker in the same process that knows a few built-in
job kinds (`echo`, `sleep`, `fail` and `shell`). In another terminal:

```
belay enqueue default echo --payload '{"hello": "world"}' --wait
belay job list
belay events tail
```

A schedule is a cron expression, a time zone and a job to enqueue:

```
belay schedule add morning-ping --cron '0 9 * * MON-FRI' --tz America/Bogota \
    --queue default --kind echo --payload '{"ping": true}'
belay schedule preview morning-ping
belay cron explain '0 9 * * MON-FRI' --tz America/Bogota
```

Workers can run anywhere that reaches the server. `belay worker` runs
the built-in handlers against a server over HTTP; the shell handler runs
the program named in a job's payload, so it has to be switched on, and
preferably limited to a list of programs:

```
belay worker --queues media --allow convert
belay enqueue media shell --payload '{"cmd": ["convert", "in.png", "-resize", "50%", "out.png"]}'
```

Everything the command line does is plain JSON over HTTP:

```
curl -d '{"queue": "emails", "kind": "send_welcome", "payload": {"user": 42}}' \
    http://127.0.0.1:7700/v1/jobs
curl -N 'http://127.0.0.1:7700/v1/events?types=job.*'
```

## Running a server

```
belay config default > belay.toml     # every setting, commented
belay config check belay.toml
belay server --config belay.toml
```

The configuration covers the listen address, the data directory and how
often the log is synced, bearer tokens and their scopes (`admin`,
`enqueue`, `work`, `read`), per-queue limits, retries and retention,
schedules and calendars, and clustering. Any setting can be overridden
from the environment, e.g. `BELAY_SERVER__LISTEN=0.0.0.0:7700`. A server
that listens on anything but a loopback address refuses to start without
tokens, unless the configuration says `auth.disabled = true`.

`belay admin shutdown` drains a server and stops it. The standard
library cannot catch signals, so Ctrl-C ends the process at once; that
is safe, since the store is crash-consistent, and jobs that were running
go back to their queues when their leases expire.

`/healthz`, `/readyz` and `/metrics` are there for load balancers and
Prometheus.

## Clusters

Several servers can form a cluster: they find each other through seed
addresses, gossip about who is alive, and split the queues and schedules
between the live nodes by hashing their names. A request about a queue
another node owns gets a `307` to that node, which the client and the
command line follow. There is no replication: a node's jobs live on its
disk and are unavailable while it is down.

## From Rust

```rust
use belay_client::{Client, ClientConfig};
use belay_json::json;
use belay_proto::{JobSpec, UniqueScope};

let client = Client::new(ClientConfig::new("http://127.0.0.1:7700"))?;
let created = client.jobs().enqueue(
    &JobSpec::new("emails", "send_welcome")
        .payload(json!({"user": 42}))
        .unique("welcome:42", UniqueScope::UntilFinished),
)?;
println!("{} {}", created.id, created.status);
```

The client implements the same `JobSource` trait as the embedded engine,
so a `belay-worker` runtime with your own handlers can lease jobs from a
server instead of running inside it.

## Building

```
cargo build
cargo test
```

## License

MIT OR Apache-2.0
