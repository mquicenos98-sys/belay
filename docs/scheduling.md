# Schedules and calendars

A schedule enqueues a job at every time its cron expression selects in its
time zone. Policies decide what happens after downtime (misfires) and while
the previous run is still going (overlap); jitter spreads schedules that
share a minute; calendars restrict fires to business hours or keep them off
holidays. This document covers all of that, and how schedules behave across
crashes and in a cluster. The expression syntax and the daylight saving
rules are in [cron.md](cron.md).

## Schedule specs

A schedule is submitted as a `ScheduleSpec`:

| field | meaning | default and limits |
|---|---|---|
| `id` | the schedule's id, `[a-z0-9][a-z0-9_.-]{0,127}` | required |
| `cron` | 5, 6 or 7 field expression, or a nickname such as `@daily` | required; at most 256 bytes; must be able to match |
| `timezone` | IANA name, fixed offset or POSIX TZ rule | `UTC`; at most 64 bytes |
| `template` | the job to enqueue on every fire, a full job spec | required |
| `misfire` | `skip`, `fire_once` or `fire_all:<max>` | `fire_once` |
| `overlap` | `allow`, `skip` or `cancel_previous` | `allow` |
| `jitter` | upper bound of a random delay added to each fire | `0s`; at most 1h |
| `paused` | a paused schedule does not fire | `false` |
| `starts_at` | do not fire before this time | none |
| `ends_at` | do not fire at or after this time; after `starts_at` | none |
| `calendar` | name of a calendar restricting fires | none |
| `description` | free text | none; at most 1024 bytes |

The template may not set `run_at`, `unique` or `workflow`, nor `meta` keys
starting with `belay.`: the scheduler sets those per fire. Every other job
field (priority, payload, timeout, tags, tenant, a `delay`...) is copied
onto each job.

The stored `Schedule` adds its history:

```json
{
  "spec": {
    "id": "nightly",
    "cron": "0 2 * * *",
    "timezone": "America/Bogota",
    "template": {"queue": "reports", "kind": "build", "priority": 0},
    "misfire": "fire_all:5",
    "overlap": "skip",
    "jitter": "30s",
    "paused": false
  },
  "next_fire_at": "2026-09-30T07:00:00.000Z",
  "last_fire_at": "2026-09-29T22:26:35.988Z",
  "last_job": "01M3QMDSAMKP8WTTRJ1NBMNCT7",
  "fire_count": 1,
  "created_at": "2026-09-29T22:26:35.741Z",
  "updated_at": "2026-09-29T22:26:35.988Z"
}
```

`next_fire_at` is the next fire time before jitter, or absent when the
schedule will never fire again (past `ends_at`, or no future match).

## Creating and managing schedules

| action | CLI | HTTP | scope |
|---|---|---|---|
| list | `belay schedule list` | `GET /v1/schedules` | `read` |
| show | `belay schedule get <id>` | `GET /v1/schedules/:id` | `read` |
| create | `belay schedule add ...` | `POST /v1/schedules` (`409` if the id is taken) | `admin` |
| create or replace | `belay schedule add ... --replace` | `PUT /v1/schedules/:id` (`201` when created) | `admin` |
| delete | `belay schedule rm <id>` | `DELETE /v1/schedules/:id` | `admin` |
| pause, resume | `belay schedule pause <id>`, `belay schedule resume <id>` | `POST /v1/schedules/:id/pause`, `/resume` | `admin` |
| fire now | `belay schedule trigger <id>` | `POST /v1/schedules/:id/trigger` | `admin` |
| preview | `belay schedule preview <id> [-n N]` | `GET /v1/schedules/:id/preview?n=` | `read` |

`belay schedule add` takes the schedule inline, reusing the job flags of
`belay enqueue`:

```console
$ belay schedule add nightly --cron '0 2 * * *' --tz America/Bogota \
    --queue reports --kind build --misfire fire_all:5 --overlap skip --jitter 30s
created schedule nightly; next fire 2026-09-30T07:00:00.000Z (in 8h)
```

The inline flags are `--cron`, `--queue`, `--kind`, `--tz` (default
`UTC`), `--misfire`, `--overlap`, `--jitter`, `--calendar`,
`--description`, `--starts-at`, `--ends-at` and `--paused`, plus the job
flags `--payload`, `--priority`, `--max-attempts`, `--retry`, `--timeout`,
`--tag`, `--tenant` and `--meta`. Alternatively `--file` loads a whole spec
from JSON or TOML, with the same fields as the API:

```toml
# schedules/nightly.toml
id = "nightly-report"
cron = "0 3 * * *"
timezone = "America/Bogota"
misfire = "fire_once"
overlap = "skip"
template = { queue = "reports", kind = "build_daily", payload = { format = "pdf" } }
```

```console
$ belay schedule add --file schedules/nightly.toml --replace
```

The CLI parses the expression and the zone before sending anything, so a
mistake points at its column. Deleting a schedule leaves the jobs it
already enqueued alone.

## Fire times and misfires

A schedule's fire times are the matches of its cron expression in its time
zone, inside `[starts_at, ends_at)`, that its calendar (if any) allows,
evaluated in the calendar's own zone. Fire times are whole seconds.

The server's scheduler thread sleeps until the earliest `next_fire_at` of
any schedule, at most `scheduler.tick` (1s by default, which bounds how
late it notices schedules changed by other means), and then processes
every due schedule. For each one it walks the due fire times from the
stored `next_fire_at` up to now:

- a fire processed within `scheduler.misfire_threshold` of its time (60s
  by default) is **on time** and fires;
- an older one is a **misfire**, for instance because belay was down or
  the scheduler stalled, and the schedule's policy decides:

| policy | missed fires |
|---|---|
| `skip` | dropped; the schedule waits for its next fire time |
| `fire_once` (default) | one fire for the lot, stamped with the first missed time |
| `fire_all:<max>` | one fire per missed time, oldest first, at most `max` (1 to 1000); plain `fire_all` means `fire_all:100` |

Missed fires beyond what the policy fires are reported as skipped with the
reason `misfire`, and a `schedule.misfired` event carries the first missed
time, the number missed and the policy. After 10,000 missed fire times the
walk stops counting and jumps to the fire times near now, so a schedule
that fires every second does not make a long outage expensive; fires within
the threshold are still visited one by one.

For example, with a schedule `0 * * * *` and the server down from 09:30 to
12:10: the 10:00, 11:00 and 12:00 fires are missed. `skip` enqueues
nothing until 13:00; `fire_once` enqueues one job stamped 10:00;
`fire_all:2` enqueues jobs for 10:00 and 11:00 and reports 12:00 as
skipped.

Every dispatched job carries `meta["belay.schedule"]` (the schedule id) and
`meta["belay.fire_time"]` (the nominal fire time, RFC 3339), and each fire
is published as `schedule.fired` and counted in
`belay_schedule_fires_total`.

## Overlap

Before a fire, the scheduler looks at the state of the job the previous
fire enqueued (`last_job`):

| policy | previous job not finished |
|---|---|
| `allow` (default) | fire anyway; runs may overlap |
| `skip` | skip this fire (reason `overlap`, event `schedule.skipped`) |
| `cancel_previous` | cancel the previous job, then fire |

A previous job that has finished, or that no longer exists (deleted by
retention), never blocks a fire. Cancelling a running job only requests
cancellation: its worker learns of it at its next heartbeat. If cancelling
fails, the error is reported and the fire happens anyway.
