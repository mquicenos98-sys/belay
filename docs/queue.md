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
