# belay

A durable job queue and cron scheduler in a single Rust binary.

I keep reaching for a background-job system on side projects and every
option means running Redis or Postgres next to it, or accepting that a
restart loses whatever was queued. belay is an attempt at the thing I
actually want: one process, its own storage, jobs that survive a crash,
retries with backoff, cron schedules that understand time zones, and an
HTTP API that any language can talk to.

No dependencies outside the standard library, on purpose.

## Status

The queue works as an embedded library. There is no server yet; you open
a store, open a `QueueEngine` on it and call it from your own process.

- [x] shared foundations: timestamps, ids, backoff, logging (`belay-common`)
- [x] JSON without serde (`belay-json`)
- [x] job and queue model (`belay-proto`)
- [x] a log-structured store with crash recovery (`belay-store`)
- [x] the queue engine: leases, retries, priorities, uniqueness, rate
      limits, fair sharing between tenants (`belay-queue`)
- [x] a worker runtime with handlers, heartbeats and graceful shutdown
      (`belay-worker`)
- [ ] cron schedules with time zones
- [ ] HTTP API and client
- [ ] a `belay` binary

## Using the queue

```rust
use std::sync::Arc;
use belay_common::system_clock;
use belay_json::json;
use belay_proto::source::NoopSink;
use belay_proto::JobSpec;
use belay_queue::{EngineOptions, QueueEngine};
use belay_store::{LogKv, Options, SyncPolicy};

let kv = LogKv::open("data", Options::new().sync(SyncPolicy::Always))?;
let queue = Arc::new(QueueEngine::open(
    Arc::new(kv),
    system_clock(),
    Arc::new(NoopSink),
    EngineOptions::new(),
)?);

queue.enqueue(
    JobSpec::new("emails", "send_welcome")
        .payload(json!({"user": 42}))
        .max_attempts(5),
)?;
```

Workers lease jobs through the `JobSource` trait, which the engine
implements directly; `belay-worker` runs registered handlers on a thread
pool and takes care of heartbeats, timeouts and retries. Call
`QueueEngine::tick` every few hundred milliseconds to promote delayed
jobs and expire lost leases.

Every state change is one atomic write to the store, so with
`SyncPolicy::Always` an acknowledged enqueue or completion survives a
crash.

## Building

```
cargo build
cargo test
```

## License

MIT OR Apache-2.0
