# Architecture

belay is a durable job queue and cron scheduler that runs as one
process: an HTTP API in front of an engine that keeps jobs, schedules and
workflow runs in its own log-structured store. This page is the map: the
crates and how they depend on each other, what happens to a request on
its way to the disk, which threads run and why, and the design decisions
that shaped all of it, with what they cost.

## Crates

The workspace has eighteen crates. Each has a crate-level doc comment
that goes into more detail (`cargo doc --workspace --no-deps --open`).

| crate | role |
|---|---|
| `belay-common` | timestamps and clocks, durations, ULIDs, a PRNG, backoff policies, CRC-32C and other hashes, a binary codec, structured logging, shutdown signals and a thread pool, atomic file writes and lock files, text tables |
| `belay-json` | JSON values, a strict RFC 8259 parser and writer, JSON Pointer, and the `ToJson`/`FromJson` traits every API and storage type implements by hand |
| `belay-conf` | the TOML subset of the configuration file, `BELAY_*` environment overlays, and a reader that reports unused keys with suggestions |
| `belay-cron` | cron expressions (5 to 7 fields, `L`, `W`, `#`, nicknames), POSIX TZ rules with a table of IANA zone names, DST-correct iteration, English descriptions |
| `belay-metrics` | counters, gauges, histograms and summaries in a registry, rendered in the Prometheus text format |
| `belay-http` | an HTTP/1.1 server (a thread per connection, router, middleware, server-sent events) and client (connection pool, redirects, event stream reader) on `std::net` |
| `belay-proto` | the domain model and wire format: job specs and jobs, the job state machine, queues, schedules, calendars, workflows, events, API request and response bodies, errors, validation and limits |
| `belay-store` | a sorted key-value store with atomic batches: a write-ahead log, snapshots, crash recovery, and an in-memory file system that simulates power loss for tests |
| `belay-cluster` | SWIM membership and failure detection over UDP, rendezvous hashing of keys over the live members |
| `belay-queue` | the queue engine: priorities, delays, leases, retries, uniqueness, concurrency and rate limits, tenant fair share, retention, long-poll leasing |
| `belay-scheduler` | the scheduler (cron schedules with misfire, overlap, jitter and calendar rules) and the workflow engine (DAGs of jobs) |
| `belay-worker` | the worker runtime: a handler registry, a poller, executor threads, heartbeats, timeouts, cancellation, graceful shutdown, built-in handlers |
| `belay-client` | a typed Rust client for every endpoint, with retries, redirects and event stream resumption |
| `belay-core` | the `Engine`: storage, queue, scheduler, workflows, event bus, metrics and cluster wired together, configured from one `Config` |
| `belay-server` | the HTTP API over an `Engine`: routes and scopes, authentication, rate limits, access logs, events, cluster redirects, shutdown |
| `belay-cli` | the `belay` binary |
| `belay-testkit` | in-process servers, clusters and workers for tests, and the end-to-end test suite |
| `belay-bench` | the benchmark harness (`belay-bench` binary) |

Dependencies point downwards only. The six foundation crates on the
bottom row depend on nothing, not even on each other:

```text
       belay-cli           belay-testkit          belay-bench
           |                     |                     |
           +--------+------------+---------------------+--------+
                    |                                           |
              belay-server                                belay-client
                    |                                           |
               belay-core                                       |
                    |                                           |
     +--------------+---------------+--------------+            |
     |              |               |              |            |
belay-queue  belay-scheduler   belay-worker  belay-cluster      |
     |              |               |              |            |
     +------+-------+-------+-------+--------------|------------+
            |               |                      |
       belay-store     belay-proto                 |
            |               |                      |
  ----------+---------------+----------------------+------------------
  belay-common  belay-json  belay-conf  belay-cron  belay-metrics  belay-http
```

The drawing merges edges to stay readable: the worker and the client
use only `belay-proto` from the middle row, the cluster only
`belay-common` from the bottom one, and the upper crates also reach down
past their neighbours (the server uses `belay-http` and `belay-metrics`,
the core `belay-conf`, `belay-cron` and `belay-metrics`). Every edge,
exactly:

| crate | depends on |
|---|---|
| `belay-common`, `belay-json`, `belay-conf`, `belay-cron`, `belay-metrics`, `belay-http` | nothing |
| `belay-proto` | common, json |
| `belay-store`, `belay-cluster` | common |
| `belay-queue` | common, json, proto, store |
| `belay-scheduler` | common, json, proto, cron, store |
| `belay-worker` | common, json, proto |
| `belay-client` | common, json, proto, http |
| `belay-core` | common, json, conf, cron, metrics, proto, store, queue, scheduler, cluster, worker |
| `belay-server` | common, json, proto, http, core, metrics |
| `belay-cli` | common, json, conf, cron, proto, store, http, client, worker, core, server |
| `belay-testkit` | common, json, proto, store, core, server, client, worker, cluster |
| `belay-bench` | common, json, cron, proto, store, queue, worker, client, core, server |

Two seams keep the layers apart. Workers consume the `JobSource` trait
from `belay-proto`, which both the engine (in process) and the HTTP
client (remote) implement, so the same worker runs embedded or across
the network. The scheduler and workflow engine create jobs only through
a `Dispatcher` trait, which `belay-core` implements over the queue, so
they can be tested against an in-memory dispatcher without a queue at
all.

## A request, from socket to disk

Take `POST /v1/jobs` with one job spec.

1. **Accept.** The `belay-http-accept` thread accepts the connection and
   hands it to a connection thread from an elastic pool (at most
   `server.max_connections`). That thread parses the request with limits
   on the request line, head and body, and keeps the connection alive
   for further requests.
2. **Middleware.** The router runs the request through, outermost
   first: request ids (`X-Request-Id`), the access log and HTTP metrics,
   the rewriting of plain-text errors into JSON error bodies, the panic
   catcher, and the body size limit.
3. **Route and access.** The route table maps the method and path to a
   handler and the scope it needs (`enqueue` here). The endpoint checks
   the bearer token by hashing it and comparing the digest with every
   configured one in constant time, then the token's rate limit.
4. **Handler.** The handler parses the body with `belay-json`, decodes
   it into a `JobSpec` and validates it (`belay-proto`); any problem
   becomes a `400` listing every violation. In a cluster it asks the
   engine who owns the queue, and answers `307` to the owner if that is
   another node.
5. **Engine.** `Engine::enqueue_request` passes the spec to the queue
   engine. Under its lock the queue engine fills in the queue's
   defaults, checks the uniqueness key, stages the new job document
   (JSON) and the index changes, and writes them as one `WriteBatch`.
6. **Store.** The log-structured store appends the batch to the active
   log segment as one CRC-framed record and, under `sync = "always"`,
   waits for an fsync, shared with any other store writer that arrived
   meanwhile, such as the scheduler (group commit). Then it applies the
   batch to its in-memory table and returns.
7. **Commit.** Only after the write succeeded does the queue engine
   apply the change to its in-memory indexes; if the write failed,
   memory is untouched and the client gets an error. It emits a
   `job.enqueued` event to the event bus (in commit order, still under
   the lock), releases the lock, and wakes long-polling lease requests
   that could take the new job.
8. **Reply.** The handler answers `201 Created` with the result and
   `Location`, the access log records the request, and the event bus
   delivers the event to its subscribers' queues, from which their
   stream threads write it to `/v1/events` clients.

Reads by id go straight to the store without the queue lock: every
change is one atomic batch, so a read sees a job before or after any
change, never halfway. A worker's lease runs the same path in reverse: it
takes the lock, picks jobs from the ready index, writes their new state
and lease in one batch, and answers; if nothing is ready and the request
has a `wait`, it sleeps on a condition variable without holding the
lock until an enqueue, promotion or release wakes it.

## Threads

The standard library has no async runtime and belay adds none: every
concurrent activity is an OS thread, and blocking is normal. A running
server has:

| thread | what it does |
|---|---|
| main | starts everything, then waits for the server to stop |
| `belay-server` | supervisor: waits for the HTTP server to drain, then stops the engine |
| `belay-http-accept` | accepts connections |
| `belay-http-conn-*` | one per open connection, from a pool that grows to `server.max_connections` and shrinks when idle; a long-polling lease or an event stream simply blocks its thread |
| `belay-queue-tick` | the queue tick, every `engine.tick` |
| `belay-scheduler` | sleeps until the next fire time (at most `scheduler.tick`) and fires what is due |
| `belay-workflows` | workflow chores, every `workflows.sweep_every` |
| `belay-housekeeping` | metrics refresh and time-based checkpoints, every `metrics.refresh` |
| `belay-store` | the store's background thread: interval syncs, size-triggered checkpoints |
| `belay-gossip-rx`, `belay-gossip-tick` | cluster membership, when clustered |

`belay dev` and programs that embed a worker add the worker's threads: a
poller, one executor per slot, a heartbeat thread and a watchdog.

Thread-per-connection is a deliberate choice for this workload: the
connections that matter (workers long-polling for jobs, dashboards
following events) spend almost all their time blocked, a thread is the
simplest correct way to block, and a few thousand threads are well
within what current systems handle. The cost is that
`server.max_connections` must be sized for every waiting worker slot and
subscriber, and memory grows with it by one thread stack each.

## The engine's background duties

`belay-core` starts the four duty threads listed above. Nothing in the
queue, scheduler or workflow engines runs by itself: each exposes a
`tick(now)` and is driven from outside, which is what makes them
deterministic under a manual clock in tests (`EngineBuilder::background
(false)` turns the threads off and `Engine::tick` runs one round of
everything).

- **Queue tick** (`engine.tick`, 250 ms): promotes scheduled and retrying
  jobs whose time has come, expires leases that ran out (the attempt
  fails with `lease expired` or `timed out after ...`), deletes finished
  jobs past their queue's retention, releases timed uniqueness claims,
  and reports workers that went silent. A lease request also promotes the
  due jobs of the queues it looks at, so a due job is leasable even
  between ticks.
- **Scheduler**: sleeps until the next fire time, then fires every due
  schedule, applying its misfire policy (after downtime), overlap policy
  (while the previous job runs), jitter, calendar and bounds. Each fire's
  job carries the uniqueness key `sched:<id>:<fire time>`, so a crash
  between dispatching a fire and recording it cannot fire it twice. In a
  cluster only the schedule's owner fires it.
- **Workflow chores** (`workflows.sweep_every`, 30 s): deletes finished
  runs past `workflows.keep_for`, and reconciles running steps with their
  jobs. Steps normally advance at once, from a hook the queue engine
  calls when a job finishes; the sweep catches up on anything missed
  while the engine was down.
- **Housekeeping** (`metrics.refresh`, 5 s): folds lease latency samples
  into `belay_lease_wait_seconds` and writes a checkpoint when
  `storage.checkpoint_interval` has passed.

Size-triggered checkpoints and interval syncs belong to the store's own
thread; cluster probes and gossip to the cluster node's.
