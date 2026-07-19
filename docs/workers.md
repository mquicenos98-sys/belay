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

## Registry, per-kind options and middleware

The `Registry` maps kinds to handlers, with optional per-kind limits:

```rust
use std::time::Duration;
use belay_worker::middleware::{LoggingMiddleware, TimingMiddleware};
use belay_worker::{handler_fn, JobContext, KindOptions, Outcome, Registry};

let timing = TimingMiddleware::new();
let registry = Registry::new()
    .register("send_email", handler_fn(|_: &mut JobContext| Ok(Outcome::done())))
    .register_with(
        "resize",
        handler_fn(|_: &mut JobContext| Ok(Outcome::done())),
        KindOptions::new().concurrency(2).timeout(Duration::from_secs(60)),
    )
    .wrap(LoggingMiddleware::new())
    .wrap(timing.clone());
```

`KindOptions::concurrency(n)` limits how many jobs of the kind run at once
on this worker; `KindOptions::timeout(d)` caps one attempt, the shorter of
it and the job's own `timeout` winning. A lease request cannot filter by
kind, so a job whose kind is at its limit waits on the worker, heartbeated
and holding a slot, until a job of its kind finishes. When every
registered kind has a limit, the worker asks for no more jobs than those
limits leave room for. Give kinds with tight limits their own queue if they
share one with high-volume work.

A registry is refused by `Worker::new` if it has no handlers, registers a
kind twice, has an empty kind name, or gives a kind a concurrency of 0 or a
zero timeout.

Middleware runs around every handler; the first added is outermost, seeing
the job first and the outcome last:

```rust
use belay_worker::middleware::Next;
use belay_worker::{HandlerError, JobContext, Middleware, Outcome};

/// Refuses jobs from suspended tenants.
struct DenyTenants(Vec<String>);

impl Middleware for DenyTenants {
    fn call(&self, ctx: &mut JobContext, next: Next<'_>) -> Result<Outcome, HandlerError> {
        let denied = ctx.job().spec.tenant.as_ref()
            .is_some_and(|t| self.0.iter().any(|d| d == t.as_str()));
        if denied {
            return Err(HandlerError::fatal("tenant is suspended"));
        }
        next.run(ctx)
    }
}
```

`middleware_fn` makes middleware from a closure. Two ship with the crate:
`LoggingMiddleware` logs a line when a job starts and one when it finishes
(failures always at warning level), and `TimingMiddleware` collects runs,
errors, total, longest and latest duration per kind. Panics, timeouts and
cancellation are handled by the worker itself, so they apply even without
middleware. `Registry::run(ctx)` runs a handler through the middleware on
the calling thread, for tests or inline execution.

## Heartbeats, progress and cancellation

Every `heartbeat_every` the heartbeat thread makes one pass over every job
the worker holds, including jobs waiting for a slot, and renews each lease
with one call. A heartbeat carries the latest progress the handler
recorded with `ctx.progress(percent, message)`; percent is clamped to
0..100 (NaN means unknown), an empty message is omitted and a long one is
cut to 1024 bytes. If the call fails for another reason than a lost lease,
the progress is kept for the next pass and the failure counted.

Cancellation is cooperative. The worker sets a job's cancellation flag
when:

| reason | trigger |
|---|---|
| `Requested` | a heartbeat answer says `"cancel_requested": true` |
| `TimedOut` | the attempt ran past its deadline |
| `ShuttingDown` | the shutdown grace period is over |
| `LeaseLost` | a heartbeat was answered `lease_lost`; any result will be dropped |

A long-running handler should check `ctx.is_cancelled()` now and then, or
sleep with `ctx.sleep_or_cancel(d)`, and return `HandlerError::cancelled()`
when told to stop. The worker replaces that error with one that says why
the job was stopped: a retryable `timeout`, a retryable `shutdown`, or,
for a requested cancellation, a non-retryable `cancelled on request`,
which the server turns into the job's `cancelled` state.

## Timeouts and panics

An attempt's budget is the shorter of the job's `timeout` and the kind's
`KindOptions::timeout`, counted on the worker's monotonic clock from the
moment the handler starts; time spent waiting for a slot does not count.
The watchdog sleeps until the nearest deadline. At the deadline it sets the
job's cancellation flag; if the handler has not returned `cancel_grace`
later, the worker reports a retryable failure of kind `timeout` ("timed
out after 30s") and abandons the job. Threads cannot be killed, so an
abandoned handler keeps its executor slot until it returns, and whatever
it returns is dropped.

The server enforces the same budget independently: a lease never runs past
`started_at + timeout`, so a worker that stops reporting still loses the
job at its deadline.

Panics are caught and reported as retryable failures of kind `panic`, with
the panic message (`handler panicked: index out of bounds ...`). A panic
may be caused by the input and recur, but it can as easily come from a
transient condition; a job that keeps panicking still ends dead once its
attempts run out, with the messages in its error history.

A result that cannot be delivered is retried up to `report_attempts`
times, with `source_backoff` between tries, on `unavailable`,
`resource_exhausted`, `internal` and `conflict` answers; heartbeats keep the
lease alive meanwhile. A `lease_lost` answer drops the result. If delivery
still fails, the error is logged and the job runs again once its lease
expires.
