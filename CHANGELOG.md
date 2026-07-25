# Changelog

All notable changes to belay are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/) (while it is at 0.x, a
minor version may break things; the notes say when it does).

## [0.5.0] - 2026-07-24

Documentation: a guide for every part of belay, a reference for the
configuration, the HTTP API and the command line, and examples to start
from.

### Added

- `docs/`: an architecture overview; references for the configuration
  file, the HTTP API and the `belay` command; an operations guide
  (installing, sync policies, backups, upgrades, monitoring, capacity,
  troubleshooting); guides to the storage engine, the queue, workers,
  cron expressions, scheduling, workflows, clustering and metrics.
- `examples/`: a single-node production configuration, a three-node
  cluster, schedules and a calendar, an ETL workflow, and a systemd
  unit. Every configuration passes `belay config check`.
- Runnable crate examples: an embedded queue (`belay-queue`), enqueueing
  over HTTP (`belay-client`), a worker with a custom handler and the
  shell handler (`belay-worker`), and an engine with its HTTP server in
  one process (`belay-core`).
- `CONTRIBUTING.md`: building, testing, benchmarks and conventions.

### Fixed

- Cluster redirects dropped the query string, so a redirected
  `GET /v1/jobs?queue=...&state=dead` returned the owner's jobs
  unfiltered, and a redirected preview lost its `?n=`.
- Every start rewrote the `[queues.*]` tables of the configuration and
  sent a `queue.configured` event per queue, even when nothing had
  changed.
- Resuming a schedule that had been paused across a restart fired a
  catch-up job for a time it had been paused through.
- A purge left one job behind at every batch boundary: purging 1000
  ready jobs deleted 997.
- The shell handler failed exit status 75 (`EX_TEMPFAIL`) permanently;
  it is retried by default, as documented.
- `belay enqueue --unique invoice:1234:until_started` took the whole
  text as the key; the scope is now split off at the last colon, so keys
  may contain colons.
- The caret under a cron parse error in `belay cron` pointed one column
  to the right of the offending field.
- `belay-bench` printed values that round up to the next unit with four
  digits (`1000us` instead of `1.00ms`).

## [0.4.0] - 2026-06-12

Test harnesses, an end-to-end suite and benchmarks, and the fixes they
turned up.

### Added

- `belay-testkit`: in-process harnesses for testing against real
  servers: `TestServer` (an engine and HTTP server on `127.0.0.1:0` with
  a manual clock, restarts and simulated crashes), `TestWorker`,
  `TestCluster` (nodes gossiping over an in-memory network, with kills
  and restarts), recording handlers, an event log collector, fixtures,
  waits with timeouts that say what they waited for, log capture, and a
  TCP proxy for cutting connections.
- The end-to-end suite in `belay-testkit/tests`: jobs, retries, queues,
  schedules, workflows, events, authentication, durability across
  crashes, and clusters, all through the HTTP API and the worker
  runtime.
- `belay-bench`: throughput and latency percentiles for the store by
  sync policy, recovery, the queue embedded and over HTTP, workers,
  cron iteration and JSON, with filters, a quick mode for CI and JSON
  output.

### Fixed

- `--url`, `--token` and `belay server --config` were silently
  overridden by `BELAY_URL`, `BELAY_TOKEN` and `BELAY_CONFIG`; the
  environment now only replaces defaults.
- Negative option values (`--priority -5`, `--tz -05:00`) were taken for
  missing values.
- `belay` exits with status 4, not 1, when the server answers `429`.
- Heartbeats extended leases past the job's `timeout`, so a stuck
  handler that kept heartbeating was never timed out.
- A `token_sha256` computed by other tools was refused for secrets whose
  length is 56 modulo 64: belay's SHA-256 got the padding wrong for such
  messages.
- `Last-Event-ID` now takes precedence over `?from=` on `/v1/events`, so
  a reconnecting event source no longer replays everything since its
  original `from`.
- Resuming the event stream from a point the buffer no longer holds
  yields a `lagged` frame instead of silently skipping events.
- `storage.checkpoint_interval` never triggered a checkpoint: each
  housekeeping round restarted its clock.
- `Engine::shutdown` now closes the store, releasing the data
  directory's lock even while HTTP connections still hold the engine;
  restarting an engine in-process on the same directory failed.
- A graceful shutdown waited out the keep-alive timeout of idle client
  connections.
- A request body of exactly `server.max_body` bytes was refused.
- Servers without authentication rate-limited all clients together as
  one anonymous token.
- `Retry-After` on `429` answers is rounded up, so waiting the advertised
  time is enough.
- The client retried a `POST` that failed on a stale keep-alive
  connection, which could enqueue a job twice; only idempotent requests
  are retried there now.
- The client let a `Retry-After` shorten its backoff but not lengthen
  it; it now waits for the longer of the two.
- The client's event stream received a frame it could not decode again
  after every reconnect instead of resuming past it.
- An encoded plus sign (`%2B`) in a query string was decoded as a space,
  so a filter on a tag like `c++` found nothing.
- Schedule `jitter` spread fires over a thousandth of the window
  (`5m` gave at most 300 ms).
- Iterating backwards from 2033, 2097 or 2161 with a year-restricted
  expression panicked in debug builds and skipped years in release
  builds.
- A worker shutting down with several jobs waiting for the same kind's
  concurrency slot could panic.
- A worker that could not reach its server logged a warning on every
  retry; it now logs the outage once, and again when the server answers.
- A gossip datagram with a huge update count made the receiving node's
  gossip thread panic, after which the other nodes declared it dead.
- Gossip packing stopped at the first update that did not fit in a
  datagram, leaving room for smaller ones unused.
- A late summary observation (a stalled thread, a clock stepped back)
  could reset the current window and discard newer samples.
- `process_max_fds` reported the hard open-files limit instead of the
  soft one that `open` fails at.

## [0.3.0] - 2026-04-17

The server. belay is now a single binary that serves an HTTP/JSON API
from its own storage, runs workers, and joins other nodes in a cluster;
the libraries of 0.1 and 0.2 are its engine.

### Added

- `belay-http`: HTTP/1.1 on `std::net`. A server that gives each
  connection a thread from an elastic pool, with keep-alive, chunked
  bodies, `Expect: 100-continue`, limits and timeouts answered with the
  right status, a router with parameters, `405` with `Allow` and
  automatic `HEAD` and `OPTIONS`, middleware (request ids, access log,
  panic catcher, CORS), and server-sent events. A client with connection
  pooling, opt-in redirects, and an event stream reader that resumes
  with `Last-Event-ID`. Plain HTTP only.
- `belay-metrics`: counters, gauges, histograms and sliding-window
  summaries with labels, rendered in the Prometheus text format, and
  process metrics.
- `belay-cluster`: SWIM membership over UDP with suspicion, dynamic
  suspicion timeouts, gossip, push-pull anti-entropy and graceful leave,
  and rendezvous hashing of keys over the live members. A pure,
  tick-driven state machine, simulated in the tests under loss and
  partitions. Ownership only: there is no replication.
- `belay-core`: the `Engine`, which wires the store, the queue, the
  scheduler, workflows, an event bus, metrics and clustering together,
  runs their periodic duties, and implements `JobSource` for in-process
  workers. Configured from one TOML file with `BELAY_<SECTION>__<KEY>`
  environment overrides, errors pointing at the offending line, and
  warnings for unknown keys.
- `belay-server`: the HTTP/JSON API, version 1: jobs, the worker
  protocol with long-polling leases, queues, schedules, calendars,
  workflows, workers, cluster members and administration. Bearer tokens
  (in the clear or as SHA-256) with `admin`, `enqueue`, `work` and `read`
  scopes, per-token rate limits, request ids and access logs,
  `/metrics`, `/healthz` and `/readyz`, an event stream at `/v1/events`
  with resume and lag frames, `307` redirects to the node that owns a
  queue or schedule, and graceful shutdown with
  `POST /v1/admin/shutdown`.
- `belay-client`: a typed Rust client for every endpoint, with retries
  of safe requests, cluster redirects, lazy pagination and resuming
  event streams. It implements `JobSource`, so the worker runtime can
  lease jobs over HTTP.
- `belay-cli`: the `belay` binary. `server` runs from a configuration
  file, `dev` runs an in-memory server with a built-in worker, `worker`
  runs the built-in handlers against a server; commands manage jobs,
  queues, schedules, calendars, workflows and events; `cron`, `config`
  and `admin store-inspect` work offline. Aligned tables or `--json`,
  and documented exit statuses.

### Fixed

- Job listings from the queue engine exposed the lease tokens of
  running jobs, letting anyone who could list jobs report on them in
  place of their workers; they are now redacted like single-job reads.
- `fire_once` stamped its catch-up job with the latest missed time
  instead of the first, so a fire retried after a crash could run twice.
- `skip_dependents` skipped only the direct dependents of a failed step;
  steps further down with an `always` or `any_failed` condition still
  ran.
- A string result placed inside a longer workflow payload string kept
  its JSON quotes.
- Paging through workflow runs newest first repeated the newest run on
  the second page.
- Tenant weights had no effect when workers leased one job at a time:
  the front tenant got a fresh turn on every lease.
- Workers renewed leases every half `lease_for` by default, leaving no
  room for a failed heartbeat; the default is now every third.
- Progress reported by a handler was lost when the heartbeat carrying it
  failed.
- A POSIX time zone rule for the last weekday of a month (`Mm.5.d`)
  could land on the 1st of the next month: Europe/Berlin's switch back
  to winter time in 2026 came out as November 1.
- A numeric last-weekday item (`5L`) ignored the dialect's weekday
  numbering: in the Quartz dialect it meant the wrong day, and `7L`
  never matched.
- Expressions that repeat within a minute through the seconds field
  (`*/10 0 1 * * *`) were not treated as periodic, and skipped the
  repeated hour when clocks went back.
- `@weekly` did not parse in the Quartz dialect.
- The description of an hour range ended on the hour ("between 09:00
  and 17:00" for `*/15 9-17 * * *`); it now ends at 17:59.
- Timestamps with fewer than three fraction digits (`.5Z`) were read as
  milliseconds rather than tenths of a second.
- A ULID containing a non-ASCII character was rejected with a misleading
  length.
- The JSON parser rejected carriage returns between tokens, so documents
  with Windows line endings failed.
- Writing JSON to a stream reported I/O errors as a formatting failure,
  losing the error kind.
- `EventFilter` treated `from` as exclusive, so resuming after
  `Last-Event-ID` dropped one event.
- The store's `wal_bytes` statistic left out the active log segment, and
  size-triggered checkpoints came up to a segment late.
- A log filter directive for a module also matched modules whose names
  merely began with it (`belay_cluster::member` matched
  `belay_cluster::membership`).
- A configuration value replaced by an environment variable was still
  reported at its old line in error messages.

## [0.2.0] - 2025-09-12

Schedules, calendars and workflows. Still an embeddable library; the
server is the next release.

### Added

- `belay-cron`: cron expressions with 5, 6 (seconds) or 7 (year) fields,
  lists, ranges, steps, names, `?`, `L`, `LW`, `nW`, `dL` and `d#n`, and the
  `@daily`-style nicknames. Vixie (OR) and Quartz day-field semantics.
- `belay-cron`: time zones from POSIX TZ rules, with an embedded table of
  about 180 IANA zone names mapped to their current rule. Fire times are
  searched field by field; a time inside a spring-forward gap fires once
  right after the gap, and a repeated time fires once on the first pass
  (periodic expressions fire on both passes, as cronie does).
- `belay-cron`: `describe()` renders an expression in English.
- `belay-scheduler`: persistent schedules with misfire policies
  (`skip`, `fire_once`, `fire_all:<max>`), overlap policies (`allow`,
  `skip`, `cancel_previous`), deterministic jitter, start and end bounds,
  pause and resume, manual triggers and previews. A crash between
  dispatching a fire and recording it cannot fire it twice.
- `belay-scheduler`: calendars that exclude dates or weekdays, or restrict
  a schedule to time windows, evaluated in the calendar's own zone.
- `belay-scheduler`: workflows — DAGs of jobs with per-step conditions,
  failure policies (`fail_workflow`, `continue`, `skip_dependents`),
  `{{steps.<name>.result}}` placeholders in payloads, cancellation, and
  recovery of step results that finished while the engine was down.

### Fixed

- The queue's tick now promotes every due job, batch after batch; it used
  to promote one batch per queue per tick.
- A rate limit with a fractional rate could refuse a token that was due
  because of floating-point error in the refill.
- `on_conflict: replace` cancelled a unique holder that was already
  running; it now leaves running holders alone.
- Snapshots written with values of 64 KiB or more could fail their own
  checksum on reopen.
- Recovery truncated damage in the last WAL segment as a torn tail even
  when later records proved it had been synced; it is now reported as
  corruption.
- Key prefixes built from strings did not escape zero bytes, so a prefix
  scan could match keys it should not.
- A job's error history kept its oldest ten entries instead of the newest.
- A calendar window wrapping midnight checked its after-midnight part
  against the wrong day.
- Workflow validation now reports the dependency cycle itself rather than
  every step that could not be ordered.
- JSON Pointer array indices with leading zeros or a sign are rejected.
- A backslash at the end of a line in a multi-line config string now
  trims blank lines that follow it, as TOML specifies.
- Worker panics with formatted messages were reported as "non-string
  panic payload".

## [0.1.0] - 2025-05-30

First release: an embeddable, durable job queue with its own storage
engine and a worker runtime, and no dependencies outside the standard
library.

### Added

- `belay-common`: millisecond timestamps with RFC 3339 parsing and
  formatting, injectable clocks, ULIDs, compact duration syntax
  (`1h30m`), backoff policies with jitter (`exp:1s*2..10m~full`),
  structured logging (logfmt or JSON), a thread pool, shutdown signals,
  CRC-32C, and atomic file writes.
- `belay-json`: a JSON parser and writer with exact 64-bit integers,
  shortest round-trip floats, JSON Pointer, `ToJson`/`FromJson` with
  path-aware errors, and a `json!` macro.
- `belay-conf`: a TOML-subset config reader with compiler-style located
  diagnostics, `BELAY_*` environment overrides, and warnings for unknown
  keys with suggestions.
- `belay-proto`: the job, queue, schedule and workflow model, its JSON
  wire format and validation, and the `JobSource` trait workers lease
  through.
- `belay-store`: a log-structured key-value store. Batches go to a
  CRC-framed write-ahead log with group commit and a choice of sync
  policy (`always`, `interval:<d>`, `never`); checkpoints write a
  snapshot and trim the log; recovery truncates a torn tail and refuses
  real corruption. Ships an in-memory file system that simulates power
  loss for tests.
- `belay-queue`: the queue engine — priorities, delayed jobs, leases with
  tokens and heartbeats, retries with backoff, dead jobs, unique jobs,
  per-queue concurrency limits and rate limits, fair sharing between
  tenants, retention, and long-poll leasing.
- `belay-worker`: a worker runtime with a handler registry, per-kind
  concurrency, heartbeats, timeouts, cancellation, graceful shutdown, and
  built-in shell, echo and sleep handlers.
