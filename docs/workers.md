# Workers

A worker leases jobs from a belay server, runs the handler registered for
each job's `kind`, keeps the leases alive with heartbeats while the
handlers run, and reports every outcome. The `belay-worker` crate is the
runtime for workers written in Rust; `belay worker` is a ready-made worker
process with the built-in handlers; and any language that can speak HTTP
and JSON can implement the same protocol. This document covers all three.

## How a worker runs

A `Worker` takes a job source, a registry of handlers and a configuration:

```rust
use std::sync::Arc;
use std::time::Duration;
use belay_client::{Client, ClientConfig};
use belay_worker::{handler_fn, HandlerError, JobContext, Outcome, Registry, Worker, WorkerConfig};

let source = Arc::new(Client::new(ClientConfig::from_env())?);
let registry = Registry::new().register(
    "send_welcome",
    handler_fn(|ctx: &mut JobContext| {
        let user = ctx.payload()["user"]
            .as_u64()
            .ok_or_else(|| HandlerError::fatal("payload has no user id"))?;
        // ... send the mail ...
        Ok(Outcome::with(belay_json::json!({"sent_to": user})))
    }),
);
let config = WorkerConfig::default()
    .queues(vec!["emails".parse()?])
    .concurrency(8);
let handle = Worker::new(source, registry, config)?.spawn()?;
// ... later ...
let report = handle.stop();
println!("{report}");
```

The source is anything implementing `JobSource`: the HTTP client
(`belay_client::Client`), or the engine itself for workers in the server's
process (`Engine::embedded_worker`, which is what `belay dev` uses).

A running worker has one poller thread, `concurrency` executor threads, a
heartbeat thread and a timeout watchdog. `Worker::run(shutdown)` blocks
until the given `Shutdown` triggers; `Worker::spawn()` runs the same on a
supervisor thread and returns a `WorkerHandle` with `stats()`,
`shutdown()`, `join()` and `stop()`.

The poller only asks for as many jobs as there are free executor slots
(at most 100 per call), so a worker never holds more leases than it has
threads. Lease calls long-poll for `poll_wait`; after an empty answer that
came back quickly (no long polling, or a source that ignores it) the
poller waits out `poll_interval` before asking again. While the source is
failing, the poller backs off with `source_backoff` and logs the outage
once at warning level and the recovery once at info.

| `WorkerConfig` field | default | limits |
|---|---|---|
| `id` | `host:pid`, from `HOSTNAME` or `COMPUTERNAME` | worker id rules, at most 128 characters |
| `queues` | `["default"]` | 1 to 32, in order of preference, no duplicates |
| `concurrency` | 4 | 1 to 1024 |
| `lease_for` | 30s | 1s to 1h |
| `poll_wait` | 10s | 0 (no long polling) to 60s |
| `poll_interval` | 500ms | |
| `heartbeat_every` | a third of `lease_for` | positive and shorter than `lease_for` |
| `shutdown_grace` | 30s | |
| `cancel_grace` | 5s | |
| `source_backoff` | `exp:250ms*2..30s~equal` | |
| `report_attempts` | 3 | at least 1 |
| `logger` | the global logger | |

`Worker::new` validates the configuration and the registry and refuses to
build a worker with a mistake in either.

Delivery is at least once: a job can run more than once after a lost
lease, a timeout past the grace period, a result that could not be
reported, or a worker crash. Handlers should be idempotent.

## Handlers and outcomes

A handler runs one attempt of a job:

```rust
pub trait Handler: Send + Sync {
    fn handle(&self, ctx: &mut JobContext) -> Result<Outcome, HandlerError>;
}
```

Handlers are shared by every executor thread; per-job state belongs in
locals or in the context. `handler_fn` turns a closure into a handler.

A successful attempt returns `Outcome::done()` (no result),
`Outcome::with(value)` or `Outcome::json(&value)`; the value is stored as
the job's `result`, and `Outcome::with(Value::Null)` stores an explicit
`null`. A result larger than 1 MiB as JSON is reported as a non-retryable
failure of kind `result_too_large` instead.

A failed attempt returns a `HandlerError`:

```rust
use std::time::Duration;
use belay_worker::HandlerError;

// Worth retrying, subject to the job's remaining attempts.
let e = HandlerError::retry("upstream answered 503").with_kind("http_503");
// Retry after exactly this long instead of the backoff policy's delay.
let e = HandlerError::retry("rate limited").retry_after(Duration::from_secs(30));
// No retry will fix it: the job goes straight to dead.
let e = HandlerError::fatal("unknown template \"welcom\"");
```

Conversions make `?` do the expected thing: a `String` or `&str` becomes a
retryable error, an `io::Error` a retryable error of kind `io`, and a JSON
decoding error a non-retryable error of kind `invalid_payload`. A `kind`
is sent as the failure's machine-readable class; characters outside
letters, digits and `_ . : / -` are replaced, and it is cut to 64
characters, rather than letting the server reject the report.

The worker itself reports these kinds:

| kind | when | retryable |
|---|---|---|
| `cancelled` | the handler stopped because cancellation was requested | no (the job ends `cancelled`) |
| `timeout` | the attempt ran past its timeout | yes |
| `panic` | the handler panicked | yes |
| `shutdown` | the worker shut down before the attempt finished | yes, with `retry_in: 0` |
| `unknown_kind` | no handler is registered for the job's kind | no |
| `invalid_payload` | the payload does not decode as the handler expects | no |
| `result_too_large` | the result exceeds 1 MiB | no |
| `io` | an I/O error converted with `?` | yes |

## JobContext

The context is everything a handler knows about its job besides its own
state:

| method | returns |
|---|---|
| `job()`, `id()`, `kind()`, `queue()` | the job as leased and its identity |
| `attempt()` | this attempt's number, from 1 |
| `is_last_attempt()` | whether a failure now would be final |
| `worker()` | the worker's id |
| `payload()` | the raw payload (`&Value`) |
| `payload_as::<T>()` | the payload decoded with `FromJson`; a mismatch is an `invalid_payload` error |
| `deadline()`, `remaining()` | when this attempt's timeout runs out, on the worker's monotonic clock |
| `progress(percent, message)`, `set_progress(p)` | records progress for the next heartbeat |
| `is_cancelled()`, `cancel_reason()`, `check_cancelled()` | whether and why the handler should stop |
| `sleep_or_cancel(d)` | sleeps, waking early on cancellation; `false` if cancelled |
| `is_shutting_down()` | whether the worker has begun a graceful shutdown |
| `cancel_handle()` | a handle that cancels the job from another thread |
| `extensions()`, `extensions_mut()` | values attached by middleware |

`JobContext::detached(job)` builds a context outside any worker, for
calling a handler directly in a test.

The `job_error!`, `job_warn!`, `job_info!`, `job_debug!` and `job_trace!`
macros log with the job's `job`, `kind` and `attempt` fields attached, so
one search finds everything an attempt logged:

```rust
use belay_worker::job_info;

job_info!(ctx, "rendered template"; template = "welcome");
// ts=... level=info target=app::email msg="rendered template" job=01JAB... kind=send_email attempt=2 template=welcome
```
