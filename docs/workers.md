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

## Graceful shutdown

`WorkerHandle::stop()` (or the `Shutdown` passed to `run` triggering)
drains the worker in four steps:

1. Leasing stops. Jobs leased but not started are failed back at once as
   retryable `worker shutting down` with `retry_in: 0`, so another worker
   can take them; a lease call still in flight is waited for, and anything
   it returns is handed back the same way.
2. Running jobs get `shutdown_grace` to finish, with heartbeats keeping
   their leases alive. Handlers can see `ctx.is_shutting_down()` and, for a
   long job, checkpoint and fail with a short `retry_after` rather than race
   the deadline.
3. Jobs still running are cancelled (`ShuttingDown`), failed back like the
   unstarted ones, and abandoned.
4. Executor threads get `cancel_grace` to return; threads still inside a
   handler after that are detached.

Keep `poll_wait` well below `shutdown_grace`: step 1 waits for an
in-flight lease call.

A handed-back job has used up an attempt (the server counted it when the
job was leased), and the protocol has no call that returns a lease without
a failure, so a job on its last attempt ends dead.

The returned `WorkerReport` has the final `WorkerStats`, the uptime,
`drained` (every running job finished within the grace period) and
`detached_threads`. `WorkerStats` counts jobs `leased`, `succeeded`,
`failed`, `retried` (failed and reported retryable), `abandoned`,
`panicked`, `timed_out`, `cancelled` and `handed_back`, errors of each
kind of call (`lease_errors`, `heartbeat_errors`, `report_errors`), and
shows `running`, `waiting`, `busy_slots`, `concurrency` and the most recent
error.

## Built-in handlers

`belay_worker::builtin` has four handlers. `builtin::defaults()` registers
the harmless ones as `echo`, `sleep` and `fail`; the shell handler is
never registered implicitly.

| kind | handler | payload | result |
|---|---|---|---|
| `echo` | `EchoHandler` | anything | the payload |
| `sleep` | `SleepHandler` | `{"for": "1.5s"}`, or `null` for the default of 1s; at most 1h | `{"slept": "1.5s"}`; reports progress every tenth of the time and stops early when cancelled |
| `fail` | `FailHandler` | optional overrides: `message`, `retryable`, `kind`, `retry_in`, `panic`, `succeed_on_attempt` | fails every attempt (`failed on purpose` by default); with `succeed_on_attempt: n`, attempts from `n` on succeed with `{"attempt": n}` |
| `shell` | `ShellHandler` | see below | see below |

The shell handler runs the command a job's payload describes:

```json
{
  "cmd": ["convert", "in.png", "-resize", "50%", "out.png"],
  "env": {"MAGICK_THREAD_LIMIT": "1"},
  "cwd": "/srv/media",
  "stdin": "text written to the command's standard input",
  "timeout": "30s",
  "retry_on_exit": [75]
}
```

| field | meaning |
|---|---|
| `cmd` | program and arguments; the program is looked up on `PATH`, and no shell is involved unless the program is one (`["sh", "-c", "..."]`) |
| `env` | extra environment variables, added to the worker's own |
| `cwd` | working directory; the worker's own when absent |
| `stdin` | text for standard input, which is otherwise empty |
| `timeout` | kill the command after this long |
| `retry_on_exit` | exit statuses worth retrying; `[75]` (`EX_TEMPFAIL`) when absent |

Only the last 64 KiB of stdout and of stderr are kept. The outcome depends
on how the command ends:

| ending | outcome |
|---|---|
| exit status 0 | success with `{"exit_code": 0, "stdout": "...", "stderr": "..."}`, plus `"stdout_truncated": true` or `"stderr_truncated": true` when output was cut |
| any other status | failure of kind `exit:<status>`, with the end of stderr (or of stdout, if stderr is empty) in the message; retryable only if the status is in `retry_on_exit` |
| killed by a signal | retryable failure of kind `signal` |
| program not found, or permission denied | non-retryable failure of kind `spawn` |
| the payload's `timeout` passed | the command is killed; retryable failure of kind `timeout` |
| the job was cancelled | the command is killed |
| program not in the allow list | non-retryable failure of kind `not_allowed` |

For example, through the worker in `belay dev`:

```console
$ belay enqueue default shell --payload '{"cmd": ["sh", "-c", "echo disk full >&2; exit 3"]}' --wait
job 01M3QMMMNS9TTZMK3G9M22MCY5 dead after 1 attempt in 23ms
error: job 01M3QMMMNS9TTZMK3G9M22MCY5 is dead: command exited with status 3: disk full
```

Running what a payload says is running arbitrary code for anyone who can
enqueue, so restrict the programs with an allow list:

```rust
use belay_worker::builtin::{self, ShellHandler};

let registry = builtin::defaults().register(
    builtin::SHELL,
    ShellHandler::new().allow(["convert", "ffmpeg"]),
);
```

Programs are compared exactly with `cmd[0]`: list `/usr/bin/convert`
separately from `convert` if both are wanted. `ShellHandler` also takes
`max_output(bytes)`, `default_timeout(d)`, `clear_env()` (start commands
with an empty environment plus the payload's `env`) and
`retry_on_exit(codes)`.

## Running belay worker

`belay worker` is a worker process that leases from a server over HTTP and
runs the built-in `echo`, `sleep` and `fail` handlers, plus `shell` when
asked for:

```console
$ belay worker --queues emails,reports --concurrency 8
$ belay worker --queues media --allow convert --allow ffmpeg
$ belay worker --queues default --burst --poll-wait 1s
```

| flag | meaning | default |
|---|---|---|
| `--queues LIST` | queues to serve, in order of preference; repeatable or comma-separated | `default` |
| `-c`, `--concurrency N` | jobs run at once | 4 |
| `--id ID` | worker id | `host:pid` |
| `--shell` | also run shell jobs, any program | |
| `--allow PROGRAM` | run shell jobs, only for this program; repeatable | |
| `--lease-for DURATION` | lease length; heartbeats renew it | 30s |
| `--poll-wait DURATION` | long-poll wait when idle | 10s |
| `--burst` | exit once the queues stay empty for one poll wait | |
| `--max-jobs N` | exit after this many jobs finished | |

The server address and token come from `--url` and `--token`, or
`BELAY_URL` and `BELAY_TOKEN`; the token needs the `work` scope. The
worker logs to stderr in logfmt; `-v` and `-q` adjust the level.

```console
$ belay worker --queues images --concurrency 2
worker host-a:4127 serving images from http://127.0.0.1:7700 with 2 slots; kinds: echo, fail, sleep
```

Without `--burst` or `--max-jobs` the worker runs until the process is
stopped. The standard library cannot catch signals, so Ctrl-C ends it at
once rather than draining; the jobs it was running return to their queues
when their leases expire. With `--burst` or `--max-jobs` it drains
gracefully before exiting and prints a summary such as `worker host-a:4127
stopped: 12 succeeded, 1 failed, 0 abandoned`.

`belay workers` lists the workers the server has heard from recently, with
their queues, active jobs and counts.

## Writing a worker in another language

The worker protocol is four HTTP calls with JSON bodies. When
authentication is on, every call needs `Authorization: Bearer <token>`
with a token that has the `work` scope. Errors come back as
`{"error": {"code": "...", "message": "...", "details": ...}}`; branch on
`code`, never on the message.

**1. Lease.** Ask for jobs, long-polling while there are none (the answer
is wrapped here to fit the page):

```console
$ curl -s -X POST http://127.0.0.1:7700/v1/lease \
    -d '{"worker": "py-worker-1", "queues": ["images"], "max_jobs": 1, "lease_for": "60s", "wait": "20s"}'
{"jobs":[{"job":{"id":"01M3QMCBNKJ9H9NQ7VZ376D17C","state":"running","attempt":1,
  "spec":{"queue":"images","kind":"resize","payload":{"src":"s3://bucket/a.png","width":640},
  "priority":0,"max_attempts":3,"retry":"exp:1s*2..1h~10%","timeout":"30m"},
  "created_at":"2026-09-29T22:25:49.235Z","updated_at":"2026-09-29T22:25:49.277Z",
  "run_at":"2026-09-29T22:25:49.235Z","started_at":"2026-09-29T22:25:49.277Z",
  "lease":{"worker":"py-worker-1","token":"6baa24b4fef0a186","acquired_at":"2026-09-29T22:25:49.277Z",
  "expires_at":"2026-09-29T22:26:49.277Z","heartbeat_at":"2026-09-29T22:25:49.277Z"}},
  "token":"6baa24b4fef0a186"}]}
```

An empty answer, `{"jobs":[]}`, means nothing was ready within `wait`; ask
again. Durations are strings (`"60s"`); a bare number such as `"lease_for":
30` is refused with `400 invalid_argument`.

**2. Heartbeat.** While the job runs, renew the lease well before it
expires (a third of `lease_for` survives one lost heartbeat), optionally
with progress:

```console
$ curl -s -X POST http://127.0.0.1:7700/v1/jobs/01M3QMCBNKJ9H9NQ7VZ376D17C/heartbeat \
    -d '{"token": "6baa24b4fef0a186", "progress": {"percent": 40, "message": "downloaded"}}'
{"expires_at":"2026-09-29T22:26:54.854Z","cancel_requested":false}
```

If `cancel_requested` is true, stop the work and report with `fail`.

**3a. Complete.** Report success with an optional result (at most 1 MiB):

```console
$ curl -s -o /dev/null -w '%{http_code}\n' -X POST \
    http://127.0.0.1:7700/v1/jobs/01M3QMCBNKJ9H9NQ7VZ376D17C/complete \
    -d '{"token": "269d0ffaffe59def", "result": {"dst": "s3://bucket/a-640.png"}}'
204
```

**3b. Fail.** Report a failed attempt. `retryable` defaults to true;
`retry_in` overrides the backoff delay (up to 365 days); `kind` follows the
label rule (letters, digits, `_ . : / -`, at most 64 characters). The
message is never refused for its length; it is cut to 4096 bytes when
recorded.

```console
$ curl -s -o /dev/null -w '%{http_code}\n' -X POST \
    http://127.0.0.1:7700/v1/jobs/01M3QMCBNKJ9H9NQ7VZ376D17C/fail \
    -d '{"token": "6baa24b4fef0a186", "error": "upstream answered 503", "kind": "http_503", "retry_in": "30s"}'
204
```

**Lost leases.** Any call with a token that no longer holds the lease is
answered `409 lease_lost`. Drop the job without reporting anything else:
it expired, or another worker has it.

```console
$ curl -s -X POST http://127.0.0.1:7700/v1/jobs/01M3QMCBNKJ9H9NQ7VZ376D17C/heartbeat \
    -d '{"token": "0000000000000001"}'
{"error":{"code":"lease_lost","message":"lease on job 01M3QMCBNKJ9H9NQ7VZ376D17C is no longer held (expired or given to another worker)","details":{"job":"01M3QMCBNKJ9H9NQ7VZ376D17C"}}}
```

A minimal worker loop, in any language:

```text
loop:
    POST /v1/lease {worker, queues, max_jobs: free slots, lease_for, wait}
    on 5xx or a connection error: back off, then retry
    for each {job, token}:
        start the handler for job.spec.kind with job.spec.payload
        every lease_for / 3 while it runs:
            POST /v1/jobs/{id}/heartbeat {token}
            409 lease_lost      -> abandon the job, report nothing
            cancel_requested    -> stop the handler, then fail
        on success: POST /v1/jobs/{id}/complete {token, result}
        on failure: POST /v1/jobs/{id}/fail {token, error, kind, retryable}
        unknown kind: fail with retryable: false
```

`503` and `429` answers carry `Retry-After`. In a cluster, a lease for
queues owned by another node is answered `307` with the owner's URL in
`Location`; follow it, and send that job's heartbeats and result to the
node that handed it out. A lease naming queues on several nodes is served
from the queues owned by the node asked, and the others are listed in the
`Belay-Remote-Queues` response header; see [cluster.md](cluster.md).

## Limitations

- Handler threads cannot be killed; a handler that ignores cancellation
  costs a slot until it returns and is detached at shutdown.
- A job handed back at shutdown has used up an attempt.
- The shell handler kills only the direct child process. A command that
  starts its own children (`sh -c "a | b"`) can leave them running after a
  timeout, and if they keep the output pipes open the handler waits at most
  a quarter of a second for end-of-file before reporting what it has.
- Heartbeats are one call per job per period; the protocol has no batch
  heartbeat.
- Deadlines use the worker's monotonic clock from the moment the handler
  starts; time spent waiting for a slot does not count.
- `belay worker` cannot drain on Ctrl-C; jobs it was running return only
  after their leases expire.
- The HTTP client does not remember cluster redirects. A Rust worker (or
  `belay worker`) pointed at a node that does not own its queues follows
  the lease redirect, but then sends heartbeats and results to the node it
  was configured with, which does not hold those jobs and answers
  `lease_lost`; the jobs are abandoned and run again after their leases
  expire. Point workers at the node that owns their queues.
