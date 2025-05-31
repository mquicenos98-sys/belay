# Changelog

All notable changes to belay are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/) (while it is at 0.x, a
minor version may break things; the notes say when it does).

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
