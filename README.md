# belay

A durable job queue and cron scheduler in a single Rust binary.

I keep reaching for a background-job system on side projects and every
option means running Redis or Postgres next to it, or accepting that a
restart loses whatever was queued. belay is an attempt at the thing I
actually want: one process, its own storage, jobs that survive a crash,
retries with backoff, cron schedules that understand time zones, and an
HTTP API that any language can talk to.

No dependencies outside the standard library, on purpose.

## Status

Still a library: the queue, the worker runtime and now the scheduler can
be embedded in a Rust program. The server comes next.

- [x] shared foundations (`belay-common`), JSON (`belay-json`), config
      (`belay-conf`)
- [x] job and queue model (`belay-proto`)
- [x] a log-structured store with crash recovery (`belay-store`)
- [x] the queue engine (`belay-queue`) and worker runtime (`belay-worker`)
- [x] cron expressions with time zones and DST handling (`belay-cron`)
- [x] schedules, calendars and workflows (`belay-scheduler`)
- [ ] HTTP API and client
- [ ] a `belay` binary

## Schedules

A schedule is a cron expression, a time zone and a job template:

```rust
let spec = ScheduleSpec::new(
    ScheduleId::new("nightly-report")?,
    "30 2 * * *",
    JobSpec::new("reports", "build_nightly"),
)
.timezone("America/Bogota")
.misfire(MisfirePolicy::FireOnce);
scheduler.create(spec)?;
```

Cron expressions take 5, 6 (with seconds) or 7 (with year) fields and
the usual extensions: `L`, `W`, `#`, names, `@daily` and friends. Times
are evaluated in the schedule's zone. A time that falls in a
spring-forward gap fires once, right after the gap; a time that happens
twice when clocks go back fires once, on the first pass.

If belay was down when a schedule should have fired, the misfire policy
decides what happens: skip it, fire once, or fire every missed time up
to a limit. Overlap policies stop a slow job from piling up behind
itself, and calendars can exclude holidays or restrict a schedule to
business hours.

## Workflows

A workflow is a small DAG of jobs. Steps run when their dependencies
finish, can read their parents' results through `{{steps.<name>.result}}`
placeholders in their payloads, and a failure either fails the whole run,
skips the steps that depended on it, or is ignored, per step.

## Building

```
cargo build
cargo test
```

## License

MIT OR Apache-2.0
