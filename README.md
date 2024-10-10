# belay

A durable job queue and cron scheduler in a single Rust binary.

I keep reaching for a background-job system on side projects and every
option means running Redis or Postgres next to it, or accepting that a
restart loses whatever was queued. belay is an attempt at the thing I
actually want: one process, its own storage, jobs that survive a crash,
retries with backoff, cron schedules that understand time zones, and an
HTTP API that any language can talk to.

It is also an excuse to build the pieces myself, so the rule is: no
dependencies outside the standard library.

Nothing works yet beyond the shared building blocks. Rough plan:

- [ ] shared foundations: timestamps, ids, backoff, logging (`belay-common`)
- [ ] JSON without serde (`belay-json`)
- [ ] job and queue model
- [ ] a small log-structured store with crash recovery
- [ ] the queue engine: leases, retries, priorities, dead jobs
- [ ] cron schedules with time zones
- [ ] HTTP API and a worker runtime
- [ ] a `belay` binary

## Building

```
cargo build
cargo test
```

## License

MIT OR Apache-2.0
