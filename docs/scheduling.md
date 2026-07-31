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

## Jitter, start and end

With `jitter`, each fire's job gets `run_at = fire time + offset`, where
the offset lies in `[0, jitter)` at millisecond resolution. It is derived
from the schedule id and the fire time by a hash, not drawn at random, so a
fire that is repeated (after a crash, say) runs at the same moment as the
original would have, and schedules sharing a minute spread out the same way
every time. The job is enqueued at the nominal time and waits in
`scheduled` until its `run_at`. A `delay` in the template is added on top
of the offset.

`starts_at` and `ends_at` clip the fire times to `[starts_at, ends_at)`.
Once past `ends_at`, `next_fire_at` becomes absent and the schedule stays
stored but never fires.

## Pause, resume, triggers and previews

A paused schedule keeps moving through its fire times without firing, so
resuming never unleashes a burst of catch-up jobs: `resume` fires next at
the first fire time after now. Replacing a schedule's definition (`PUT` or
`--replace`) keeps its history (fire count, last fire and job) and
recomputes the next fire time from now, so fires missed before the update
are not caught up either.

`trigger` fires the schedule now, whatever its expression, calendar,
bounds, pause state or overlap policy say. The job's fire time is now, its
`meta["belay.trigger"]` is `manual`, its uniqueness key gets a `:manual`
suffix so it cannot collide with a regular fire in the same millisecond,
and it becomes the schedule's last job. The next regular fire time does not
change.

```console
$ belay schedule trigger nightly
fired nightly: enqueued 01M3QMDSAMKP8WTTRJ1NBMNCT7
```

`preview` lists the next fire times (10 by default, at most 1000),
starting with the stored `next_fire_at`, which may already be due. Paused
schedules are previewed as if they were running. Calendars are applied;
jitter is not:

```console
$ belay schedule preview standup -n 4
TIME                  UTC                       IN
2026-09-30T07:00:00Z  2026-09-30T07:00:00.000Z  in 8h
2026-09-30T07:30:00Z  2026-09-30T07:30:00.000Z  in 9h
2026-09-30T08:00:00Z  2026-09-30T08:00:00.000Z  in 9h
2026-09-30T08:30:00Z  2026-09-30T08:30:00.000Z  in 10h
```

The first column is in the schedule's zone. To try an expression without
creating anything, use `belay cron explain` or `belay cron next`.

## Calendars

A calendar is a named set of rules for when schedules may fire, such as
"weekdays 09:00-17:00, not on public holidays". A schedule names at most one
calendar; many schedules may share one.

| field | meaning | default and limits |
|---|---|---|
| `name` | the calendar's name, `[a-z0-9][a-z0-9_.-]{0,127}` | required |
| `timezone` | zone the rules are read in | `UTC` |
| `exclude_dates` | dates (`YYYY-MM-DD`) on which nothing fires | at most 1000 |
| `exclude_weekdays` | weekdays on which nothing fires | may not exclude all seven |
| `windows` | allowed daily time ranges | none (the whole day); at most 64 |
| `description` | free text | at most 1024 bytes |

A local date and time is allowed when its date is not excluded, its
weekday is not excluded, and it falls inside at least one window (or there
are no windows). A window is `{"days", "start", "end"}`, the range
`[start, end)` in local time with minute precision (`HH:MM`). `days` names
the weekdays the window *starts* on, and an empty or absent `days` means
every day. A window whose end is not after its start runs past midnight:
`22:00-06:00` on `fri` covers Friday 22:00 to Saturday 06:00. `24:00` is
accepted as an end, `start` and `end` must differ, and `00:00-24:00` is the
whole day.

Weekday sets are written as comma-separated days or ranges, `mon-fri`,
`sat,sun`, `fri-mon` (ranges may wrap), with full or three-letter names in
any case. In JSON they are arrays whose items may themselves be ranges, and
a single string is accepted too; output is always an array of three-letter
lowercase names.

```json
{
  "name": "business-hours",
  "timezone": "Europe/Berlin",
  "exclude_dates": ["2026-12-25"],
  "windows": [{"days": ["mon-fri"], "start": "09:00", "end": "17:00"}]
}
```

```console
$ belay calendar put business-hours --tz Europe/Berlin \
    --window 'mon-fri 09:00-17:00' --exclude-date 2026-12-25
saved calendar business-hours
$ belay schedule add standup --cron '*/30 * * * *' --calendar business-hours \
    --queue ops --kind remind
```

The calendar is evaluated in its own zone, which may differ from the
schedule's: a schedule in `America/New_York` can use a calendar of Berlin
business hours. A cron match that the calendar does not allow is *vetoed*:
it is not a fire time. The search walks past vetoed matches, and after
10,000 in a row jumps straight to the next moment the calendar opens, so an
every-second expression under a weekday calendar costs no more than an
hourly one. A calendar closed for more than about three years counts as
closed for good.

A schedule whose expression the calendar never allows is refused when it
is created ("no time matching the cron expression is allowed by calendar
..."). Replacing a calendar (`PUT /v1/calendars/:name`, `belay calendar
put`) recomputes the next fire time of every schedule that uses it in the
same write; an overdue fire stays due if the new calendar still allows it,
and due fires the new calendar vetoes are reported as skipped with the
reason `calendar`. A calendar cannot be deleted while a schedule uses it
(`409 failed_precondition`, naming the schedules).

| action | CLI | HTTP | scope |
|---|---|---|---|
| list | `belay calendar list` | `GET /v1/calendars` | `read` |
| show | `belay calendar get <name>` | `GET /v1/calendars/:name` | `read` |
| create or replace | `belay calendar put <name> [--tz] [--window]... [--exclude-date]... [--exclude-weekdays]` or `--file` | `PUT /v1/calendars/:name` | `admin` |
| delete | `belay calendar rm <name>` | `DELETE /v1/calendars/:name` | `admin` |

## Crash safety and deduplication

Every job a schedule dispatches carries the uniqueness key
`sched:<id>:<fire time in ms>`, held for `scheduler.dedupe_window` (24
hours by default) with `on_conflict: return_existing`. A tick dispatches
the due fires of every schedule, then saves all its bookkeeping (next fire
times, last jobs, counts) in one batch, and emits its events only once
that is saved. Fire selection is deterministic from the stored
`next_fire_at`, so if belay stops after dispatching but before saving, the
next tick repeats the same fires, and the queue hands back the jobs it
already has instead of creating second ones. The same holds when the save
itself fails: the schedules retry after `scheduler.retry_delay` (5s).

When the queue cannot take a job (`unavailable`), the schedule stays at
that fire time and retries after `retry_delay`; later fires wait behind it.
When the queue rejects a job outright, for instance because the template
is no longer valid, that fire is dropped and the error logged.

```toml
[scheduler]
enabled = true            # false stops this node from firing schedules
tick = "1s"               # longest sleep between checks
misfire_threshold = "1m"  # later than this counts as missed
dedupe_window = "1d"      # 1s to 30d
retry_delay = "5s"        # after the queue could not take a fire
```

## Configuration file versus the API

Schedules and calendars can be defined in the configuration file, and are
created or updated when the server starts:

```toml
[[calendars]]
name = "business-hours"
timezone = "America/Bogota"
exclude_dates = ["2025-12-25", "2026-01-01"]
windows = [{ days = ["mon-fri"], start = "08:00", end = "18:00" }]

[[schedules]]
id = "hourly-sync"
cron = "0 * * * *"
calendar = "business-hours"
[schedules.template]
queue = "sync"
kind = "sync_accounts"
max_attempts = 3
timeout = "10m"
```

The tables take exactly the fields of the JSON bodies and reject unknown
ones. Loading the file already parses each expression and zone and refuses
duplicate ids and names, pointing at the offending line.

At startup each file-defined schedule is compared with the stored one. An
identical definition is left alone, so restarting with an unchanged file
writes nothing. A changed one replaces the stored definition, keeping the
history and recomputing the next fire time from now. Schedules created
through the API live alongside the file's, and removing a schedule from
the file does not delete it.

Because the file is applied on every start, a change made through the API
to a schedule that the file also defines lasts only until the next
restart. That includes pausing: `paused` is part of the definition, so a
file-defined schedule paused through the API is resumed by a restart unless
the file says `paused = true`.

## Clusters

In a cluster each schedule is owned by one live node, chosen by rendezvous
hashing of the key `schedule:<id>` (see [cluster.md](cluster.md)), and fires
only there. The other nodes move it past its due fire times without firing,
reporting or emitting anything. A node fires nothing until it has joined
the cluster through a seed, or until `cluster.join_grace` (10s) has passed,
so that a node whose seeds are all down still runs.

Every API call about one schedule is redirected (`307`) to its owner, and
`belay schedule list` shows the schedules the node asked has stored. There
is no replication: a schedule created through the API is stored only on the
node that owned it at that moment. If that node goes away, the new owner
has nothing to fire. In a cluster, define schedules and calendars in the
configuration file shared by every node, so that every node stores them and
whichever node owns a schedule can fire it.

When a node takes over a schedule it continues from the present: a fire
that the previous owner missed while dying is not caught up. Jobs a
schedule creates go into the queue of the node that fires it, even if
another node owns that queue; give schedule jobs queues that the same node
owns, or run workers against every node. Calendars belong to no node: each
node checks a schedule against its own stored calendars.

## Limitations

- Time zones follow belay-cron: only each zone's current rule is known.
- Deduplication of a repeated fire relies on the fire's uniqueness key,
  which lasts for `dedupe_window`. A crash between dispatching and saving
  followed by a longer outage can produce a second job under `fire_once` or
  `fire_all`.
- Calendars have minute resolution.
- The retry delay after a failed dispatch is kept in memory; a restart
  retries at once.
- A schedule whose stored definition no longer compiles (a calendar
  removed by hand from the store, a zone missing from a newer table) is
  kept and logged but does not fire until it is updated.
- In a cluster, schedules are not replicated, ownership can briefly
  overlap while membership views converge, and a takeover does not catch up
  missed fires.
