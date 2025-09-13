# Changelog

All notable changes to belay are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/) (while it is at 0.x, a
minor version may break things; the notes say when it does).

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
