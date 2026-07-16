# Jobs and queues

A job is a unit of work: a handler name (`kind`), a JSON payload, and the
rules for running it. Jobs live in named queues; workers lease them, run
them and report the outcome. This document covers the life of a job, how
leases keep a crashed worker from losing work, retries, uniqueness,
priorities, per-queue limits, fair sharing between tenants, and what an
operator can do about a job or a queue. The engine behind all of it is the
`belay-queue` crate.

## Jobs

A job is submitted as a `JobSpec`. Every field except `queue` and `kind`
is optional:

| field | meaning | default and limits |
|---|---|---|
| `queue` | queue name, `[a-z0-9][a-z0-9_.:-]{0,63}` | required |
| `kind` | handler name workers dispatch on | required; letters, digits, `_ . : / -`, at most 128 characters |
| `payload` | handler input, any JSON value | `null`; at most 1 MiB as compact JSON |
| `priority` | higher runs first | 0; `-1000..=1000` |
| `run_at` | do not run before this time | exclusive with `delay` |
| `delay` | do not run until this long after enqueue | at most 365 days |
| `max_attempts` | attempts in total, the first included | queue default (25); `1..=1000` |
| `retry` | backoff between attempts | queue default |
| `timeout` | execution budget of one attempt | queue default (30m); 1s to 7 days |
| `unique` | deduplication rule, see below | none |
| `tags` | labels for filtering | at most 16, 64 characters each, no duplicates |
| `tenant` | owner, for fair sharing within the queue | none |
| `meta` | string annotations | at most 32 entries; keys up to 64 characters, values up to 1024 bytes; `belay.` keys are set by belay |

Unset `max_attempts`, `retry` and `timeout` are filled in from the queue's
configuration when the job is enqueued, so a stored job always carries the
values it will run with, even if the queue's configuration changes later.

JSON follows one set of conventions everywhere: timestamps are RFC 3339 in
UTC with milliseconds, durations are strings such as `"30s"` or `"1h30m"`
(bare JSON numbers are rejected), enums are lowercase strings, and unknown
fields in anything a client sends are rejected with the closest known
name. This is a stored job after one failed attempt:

```json
{
  "id": "01M3QMCBNKJ9H9NQ7VZ376D17C",
  "state": "retrying",
  "attempt": 1,
  "spec": {
    "queue": "images",
    "kind": "resize",
    "payload": {"src": "s3://bucket/a.png", "width": 640},
    "priority": 0,
    "max_attempts": 3,
    "retry": "exp:1s*2..1h~10%",
    "timeout": "30m"
  },
  "created_at": "2026-09-29T22:25:49.235Z",
  "updated_at": "2026-09-29T22:25:55.018Z",
  "run_at": "2026-09-29T22:26:25.018Z",
  "started_at": "2026-09-29T22:25:49.277Z",
  "errors": [
    {
      "at": "2026-09-29T22:25:55.018Z",
      "attempt": 1,
      "message": "upstream answered 503",
      "kind": "http_503",
      "retryable": true,
      "worker": "py-worker-1"
    }
  ]
}
```

`attempt` counts attempts started (0 until the first lease). `run_at` is
when the job becomes eligible: the spec's run time at first, then the end
of each backoff delay. `errors` keeps the newest 10 failures, each message
cut to 4096 bytes. A running job also carries its `lease` and, if a
handler reported any, its `progress`; a succeeded one carries `result`.
Jobs returned by read endpoints never include a lease token.

## Lifecycle

```text
  enqueue, run_at in the future          enqueue, due now
               |                                |
               v                                v
        +-------------+  run_at reached   +-----------+
        |  scheduled  | ----------------> |   ready   | <-------------+
        +-------------+    or promote     +-----------+               |
                                                | lease               | backoff over
                                                v                     | or promote
                                          +-----------+         +------------+
                                          |  running  | ------> |  retrying  |
                                          +-----------+  fail   +------------+
                                           |    |    |   or expiry,
                                  complete |    |    |   attempts left
                        +------------------+    |    +-------------------+
                        v                       v                        v
                  +-----------+          +-------------+            +--------+
                  | succeeded |          |  cancelled  |            |  dead  |
                  +-----------+          +-------------+            +--------+
                                   cancel requested, then     fail or expiry with no
                                   fail or lease expiry       attempts left, or fatal

  Not drawn: cancel moves scheduled, ready and retrying jobs straight to
  cancelled; retry moves dead and cancelled jobs back to ready.
```

The authoritative table, as `JobState::successors` defines it:

| from | to |
|---|---|
| `scheduled` | `ready` (run time reached, or promoted), `cancelled` |
| `ready` | `running` (leased), `cancelled` |
| `running` | `succeeded` (completed), `retrying` (failed with attempts left), `dead` (failed for good), `cancelled` (cancellation requested, then acknowledged) |
| `retrying` | `ready` (backoff elapsed, or promoted), `cancelled` |
| `succeeded` | nothing |
| `dead`, `cancelled` | `ready` (explicit retry) |

Nothing moves back into `scheduled`: a job enters there or in `ready`.
`succeeded`, `dead` and `cancelled` are terminal. Lease expiry is a failed
attempt like any other.

Every change is published on the event stream as `job.enqueued`,
`job.started`, `job.progress`, `job.succeeded`, `job.failed` (an attempt
failed and will be retried), `job.dead`, `job.cancelled`, `job.expired`,
`job.retried`, `job.promoted` or `job.deleted`.

## Enqueueing, priorities and delayed jobs

`POST /v1/jobs` takes a single spec or `{"jobs": [...]}` with up to 1000
specs. A batch is atomic: every spec is accepted (as created, duplicate or
replaced) and written in one store batch, or none is, and later specs see
earlier ones. The answer has one result per spec, and is `201 Created` when
at least one job was created (with `Location` for a single spec), `200 OK`
when every spec matched an existing job:

```console
$ curl -s -X POST http://127.0.0.1:7700/v1/jobs \
    -d '{"queue": "images", "kind": "resize", "payload": {"src": "s3://bucket/a.png"}}'
{"results":[{"id":"01M3QMCBNKJ9H9NQ7VZ376D17C","status":"created","state":"ready"}]}
```

From the command line:

```console
$ belay enqueue emails send_welcome --payload '{"user": 42}' --priority 5
$ belay enqueue reports build --delay 10m --tag nightly --meta team=finance
$ belay enqueue default echo -d @payload.json --wait
```

`--payload` (`-d`) takes inline JSON, `@FILE` or `-` for stdin. `--wait`
polls until the job finishes, prints its result and exits with status 1 if
it died.

A queue exists from its first enqueue (or its first pause or
configuration) with the default configuration. There is no separate
create step.

**Order.** Within a queue (and within one tenant, see below) ready jobs are
handed out by priority, highest first, then by how long they have been
eligible (`run_at`), then by id, which is creation order. Priority orders
jobs inside a tenant's turn; it does not let one tenant overtake another.

**Delayed jobs.** A job with a `run_at` or `delay` in the future starts
`scheduled`. The queue tick (every `engine.tick`, 250 ms by default) moves
due jobs to `ready`, and a lease request also promotes the due jobs of the
queues it names, so a job whose time has come is leasable as soon as a
worker asks, whether or not the tick has run. `belay job promote <id>`
makes a scheduled or retrying job ready at once. `run_at` and `delay` are
mutually exclusive.

## Leases, tokens and heartbeats

A worker asks for jobs with `POST /v1/lease`:

| field | meaning | default and limits |
|---|---|---|
| `worker` | worker id, `[A-Za-z0-9][A-Za-z0-9_.:@/-]{0,127}` | required |
| `queues` | queues to take from, no duplicates | required; 1 to 32 |
| `max_jobs` | most jobs to hand out | 1; at most 100 |
| `lease_for` | lease length | 30s; 1s to 1h |
| `wait` | long-poll up to this long when nothing is ready | none; at most 60s |

Each leased job moves to `running`, its `attempt` goes up by one, and it
comes with a fresh lease token of 16 hex digits. The lease lasts
`lease_for`, but never beyond the attempt's deadline, `started_at +
timeout`. Unknown queues are skipped rather than refused, so workers may
start before anything has been enqueued.

With `wait`, a request that finds nothing sleeps until something could
produce work (an enqueue, a promotion, a resume, capacity freed on a
limited queue) or until the earliest timer or rate-limit token it knows
of, and returns as soon as it has at least one job, or empty when the wait
runs out.

When a request names several queues, `engine.queue_order` decides how
picks spread over them: `round_robin` (the default) takes one job from each
queue in turn, starting from a position that rotates between requests, so
a flooded queue cannot starve the others; `in_order` drains the queues in
the order the request lists them.

Every later call about the job names it and presents the token:

| call | body | answer |
|---|---|---|
| `POST /v1/jobs/:id/heartbeat` | `{"token", "extend_by"?, "progress"?}` | `{"expires_at", "cancel_requested"}` |
| `POST /v1/jobs/:id/complete` | `{"token", "result"?}` | `204` |
| `POST /v1/jobs/:id/fail` | `{"token", "error", "kind"?, "retryable"?, "retry_in"?}` | `204` |

A heartbeat extends the lease to `extend_by` from now (1s to 1h), or by
its previous renewal length when `extend_by` is absent. It never shortens
a lease and never extends it past the attempt's deadline, so a handler that
overruns its timeout loses its lease there even while it keeps
heartbeating. A heartbeat may carry `progress` (`percent` from 0 to 100
and a `message` up to 1024 bytes), which is stored on the job and
published as `job.progress`.

The token is what protects a job from a slow worker. A call is refused with
`409 lease_lost` unless the job is running on that very token and the lease
has not run out; after an expiry the job may already be running elsewhere,
and a late report from the first worker must not overwrite the second's.
A job that no longer exists is also `lease_lost`. The worker's only
sensible reaction in both cases is to drop the job.

```json
{"error": {"code": "lease_lost",
           "message": "lease on job 01M3QMCBNKJ9H9NQ7VZ376D17C is no longer held (expired or given to another worker)",
           "details": {"job": "01M3QMCBNKJ9H9NQ7VZ376D17C"}}}
```

A lease that runs out is found by the queue tick and fails the attempt
with the error `lease expired` (kind `lease_expired`), or `timed out after
<timeout>` (kind `timeout`) when the job's own timeout is what ended it.
Both are retryable: the job retries, dies or is cancelled like any failed
attempt, and a `job.expired` event names the worker that held it.
Completing a job whose cancellation was requested still succeeds: the work
is done.

## Failures, retries and backoff

A failed attempt (`fail`, or lease expiry) moves the job to:

- `cancelled`, if cancellation had been requested;
- `retrying`, if the error is retryable (`retryable` defaults to true) and
  attempts remain (`attempt < max_attempts`), eligible again after
  `retry_in` if the worker gave one, or else after the delay of the job's
  backoff policy;
- `dead` otherwise.

Backoff policies have a compact text form, used in JSON, the configuration
file and CLI flags:

```text
const:5s                 5s every time
linear:1s+2s..1m         1s, 3s, 5s, ... capped at 1m
exp:1s*2..10m            1s, 2s, 4s, ... capped at 10m
exp:500ms*1.5            uncapped (the ..max part is optional)
exp:1s*2..10m~full       with jitter: ~full, ~equal, ~none or ~<p>%
```

`constant:` and `exponential:` are accepted as long spellings. The factor
of `exp` must be at least 1, and a cap below the initial delay is refused.
Retry number 1 is the retry after the first failed attempt. Jitter spreads
simultaneous failures:

| jitter | delay |
|---|---|
| `~none` | the base delay |
| `~full` | uniform in `[0, d]` |
| `~equal` | uniform in `[d/2, d]` |
| `~10%` | uniform in `[0.9 d, 1.1 d]`; the only mode that can exceed the cap |

The default queue policy is `exp:1s*2..1h~10%`: roughly 1s, 2s, 4s and so
on, capped at an hour. With the default 25 attempts a job that keeps
failing spends about 13 hours retrying (4,095 seconds for the first twelve
retries, then twelve hour-long waits) before it dies.

A job's `retry` is filled from the queue's `default_retry` when it is
enqueued, so changing the queue default affects jobs enqueued afterwards.

**Dead jobs** stay in the queue for the queue's dead retention (7 days by
default) so that someone can look at them and retry them:

```console
$ belay job list --queue emails --state dead
$ belay job retry 01M3QMCBNKJ9H9NQ7VZ376D17C
$ belay job list -q --state dead | xargs belay job retry
```

A retry sends a dead or cancelled job back to `ready` with a fresh budget
of attempts (`attempt` restarts at 0); its error history is kept and its
result cleared.

## Unique jobs

`unique` deduplicates enqueues: at most one job per queue holds a key at a
time.

```json
"unique": {"key": "user:42", "scope": "until_finished", "on_conflict": "return_existing"}
```

The object may be shortened to the key alone, `"unique": "user:42"`, which
takes the defaults. Keys are up to 256 bytes, non-empty and without control
characters. The scope says how long the key stays claimed:

| scope | the key is held |
|---|---|
| `until_started` | while the job is scheduled or ready |
| `until_finished` (default) | until the job reaches a terminal state |
| `for:<duration>`, e.g. `for:1h` | for that long after enqueue, whatever happens to the job, even after retention deletes it; 1s to 30 days |

`on_conflict` says what an enqueue meeting a held key does:

| policy | holder scheduled, ready or retrying | holder running or finished |
|---|---|---|
| `reject` | `409 already_exists` | `409 already_exists` |
| `return_existing` (default) | status `duplicate`, the holder's id | status `duplicate`, the holder's id |
| `replace` | holder cancelled, new job with status `replaced` | status `duplicate`, the holder's id |

`return_existing` makes an enqueue idempotent, which is also what lets the
client retry enqueues with a key safely. A rejected enqueue's error names
the holder in its details (`{"queue", "key", "job"}`).

Claims are taken at enqueue only. A job sent back to `ready` by a retry,
by hand or after a failure, does not claim its key again: it may have been
taken by a newer job in the meantime.

From the command line, `--unique KEY[:SCOPE]` and `--on-conflict POLICY`:

```console
$ belay enqueue reports build --delay 10m --unique daily-report:for:1h
enqueued 01M3QMD2P744DNSQRGVEYWD4WM on reports (scheduled)
$ belay enqueue reports build --delay 10m --unique daily-report:for:1h
job 01M3QMD2P744DNSQRGVEYWD4WM already holds the unique key; nothing enqueued (scheduled)
$ belay enqueue reports build --unique daily-report:for:1h --on-conflict reject
error: unique key "daily-report" in queue reports is held by job 01M3QMD2P744DNSQRGVEYWD4WM
```

Keys often contain colons themselves, so the CLI only splits off a
recognisable scope at the end (`:until_started`, `:until_finished`,
`:for:<duration>`).
