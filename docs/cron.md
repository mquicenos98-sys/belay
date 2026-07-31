# Cron expressions

Schedules fire at the times their cron expression selects, evaluated in a
time zone. The `belay-cron` crate parses the expressions, knows the zones,
and finds fire times field by field rather than second by second, with
explicit rules for the hours that daylight saving time skips or repeats.
This is the reference for the syntax, the zones and those rules; how
schedules use them is in [scheduling.md](scheduling.md).

Every example output below comes from `belay cron`, which runs offline
with the same parser and zone rules as the server.

## Fields

An expression has five, six or seven fields separated by spaces:

```text
+------------ second        (optional, first of six or seven fields)
| +---------- minute
| | +-------- hour
| | | +------ day of month
| | | | +---- month
| | | | | +-- day of week
| | | | | | +- year         (optional, seventh field)
| | | | | | |
* * * * * * *
```

| fields given | layout |
|---|---|
| 5 | minute, hour, day of month, month, day of week |
| 6 | second, minute, hour, day of month, month, day of week |
| 7 | second, minute, hour, day of month, month, day of week, year |

With five fields the seconds field is `0`: a five-field expression fires
at the top of the minute.

| field | values | names |
|---|---|---|
| second | 0-59 | |
| minute | 0-59 | |
| hour | 0-23 | |
| day of month | 1-31 | |
| month | 1-12 | `JAN`-`DEC` |
| day of week | 0-7, where both 0 and 7 are Sunday | `SUN`-`SAT` |
| year | 1970-2199 | |

Names are case-insensitive. Expressions are limited to 256 bytes in a
schedule.

## Values, ranges, steps and lists

| syntax | meaning | example |
|---|---|---|
| `*` | every value | `* * * * *`: every minute |
| `a` | one value | `30 9 * * *`: at 09:30 |
| `a-b` | a range | `0 9-17 * * *`: on the hour from 09:00 to 17:00 |
| `*/n` | every `n`th value from the lowest | `*/15 * * * *`: every 15 minutes |
| `a-b/n` | every `n`th value in a range | `10-50/20 * * * *`: at minutes 10, 30 and 50 |
| `a/n` | every `n`th value from `a` to the top of the field | `5/10 * * * *`: at minutes 5, 15, ..., 55 |
| `x,y,z` | a list of any of the above | `0 8,12,18 * * *` |

A range whose end is below its start wraps around the field: `FRI-MON` is
Friday through Monday, `22-2` in the hour field is 22:00 through 02:00,
`NOV-FEB` is November through February. Wrapping is an error in the year
field.

`Display` prints a normalised expression, which is what `belay cron
explain` shows as `normalized` when it differs from the input:

```console
$ belay cron explain "0,30 9-17 * * mon-fri" --json
{
  "expression": "0,30 9-17 * * mon-fri",
  "normalized": "*/30 9-17 * * MON-FRI",
  "description": "every 30 minutes between 09:00 and 17:59 on Monday through Friday",
  "timezone": "UTC",
  "next": [...]
}
```

Two expressions are equal when they select the same times, however they
are spelled: `*/15` and `0,15,30,45` compare equal.

Mistakes are reported with the field and the column:

```console
$ belay cron explain "0 25 * * *"
error: invalid cron expression: invalid value 25 in hour field (0-23) at column 3
  0 25 * * *
    ^
```

## Day-of-month and day-of-week specials

The two day fields accept more than plain values:

| syntax | field | meaning | example, and its description |
|---|---|---|---|
| `?` | either day field | same as `*`; for Quartz-style expressions | `15 10 ? * 7`: at 10:15 on Sunday |
| `L` | day of month | the last day of the month | `0 0 L * *`: at 00:00 on the last day of the month |
| `L-n` | day of month | `n` days before the last day, `n` from 1 to 30 | `0 0 L-2 * *`: at 00:00 on 2 days before the last day of the month |
| `nW` | day of month | the weekday (Monday-Friday) nearest day `n`, never leaving the month | `0 12 15W * *`: at 12:00 on the weekday nearest day 15 of the month |
| `LW` | day of month | the last weekday (Monday-Friday) of the month | `0 0 LW * *`: at 00:00 on the last weekday of the month |
| `dL`, `NAMEL` | day of week | the last weekday `d` of the month | `0 18 * * FRIL`: at 18:00 on the last Friday of the month |
| `d#n` | day of week | the `n`th weekday `d` of the month, `n` from 1 to 5 | `0 9 * * MON#1`: at 09:00 on the first Monday of the month |

`nW` stays inside the month: `1W` on a Saturday is Monday the 3rd, not
Friday of the month before. None of the specials takes a step.

When **both** day fields are restricted (neither is `*` or `?`), a day
matches if **either** field matches, as in Vixie cron and cronie:

```console
$ belay cron explain "0 0 13 * FRI" --from 2026-01-01T00:00:00Z -n 3 --json
{
  "expression": "0 0 13 * FRI",
  "normalized": "0 0 13 * FRI",
  "description": "at 00:00 on day 13 of the month or on Friday",
  "timezone": "UTC",
  "next": [
    "2026-01-02T00:00:00Z",
    "2026-01-09T00:00:00Z",
    "2026-01-13T00:00:00Z"
  ]
}
```

An expression that no date can satisfy, such as `0 0 30 2 *` (February
30th), is recognised at once: it never fires, and a schedule with it is
refused ("can never match (no date satisfies its day and month fields)").

## Nicknames and dialects

A nickname stands alone in place of the fields:

| nickname | same as |
|---|---|
| `@yearly`, `@annually` | `0 0 1 1 *` |
| `@monthly` | `0 0 1 * *` |
| `@weekly` | `0 0 * * SUN` |
| `@daily`, `@midnight` | `0 0 * * *` |
| `@hourly` | `0 * * * *` |

The parser has two dialects:

| dialect | fields | day of week | both day fields restricted |
|---|---|---|---|
| Vixie (default) | 5, 6 or 7 | 0-7, 0 and 7 are Sunday | a day matches if either field does |
| Quartz | 6 or 7 (seconds required) | 1-7, 1 is Sunday | a day matches only if both fields do |

The dialect and the day-matching rule can be chosen independently when the
crate is used as a library:

```rust
use belay_cron::{CronExpr, DayMatch, ParseOptions};

let friday_13th = CronExpr::parse_with("0 0 13 * FRI", ParseOptions::new().with_day_match(DayMatch::Both))?;
let quartz = CronExpr::parse_with("0 15 10 ? * 2-6", ParseOptions::quartz())?;
```

Schedules, calendars and `belay cron` always use the default: the Vixie
dialect with either-field day matching.

## Time zones

A zone is written in one of these forms, wherever a schedule, a calendar
or `--tz` takes one:

| form | examples | meaning |
|---|---|---|
| IANA name | `America/New_York`, `Europe/Berlin`, `Asia/Kolkata`, `America/Bogota` | the zone's current rule from the embedded table |
| UTC | `UTC`, `Etc/UTC`, `GMT`, `Etc/GMT`, `Etc/Universal`, `Etc/Zulu`, `Z` | UTC |
| fixed offset | `+05:30`, `-0800`, `+01`, `+05:30:00` | a constant offset east of UTC, at most 24:59:59 |
| POSIX TZ rule | `EST5EDT,M3.2.0,M11.1.0`, `CET-1CEST,M3.5.0,M10.5.0/3`, `<+0330>-3:30` | a rule with or without daylight saving time |
| `:` and a name | `:Europe/Paris` | a name, as in the `TZ` variable |

The embedded table maps 184 IANA names to the POSIX rule that tzdata 2026d
emits as each zone's footer, i.e. the rule in force today. Names are
case-sensitive, exactly as in the tz database; a near miss gets a
suggestion:

```console
$ belay cron explain "0 9 * * *" --tz Europe/Berln
error: invalid value "Europe/Berln" for --tz: unknown time zone "Europe/Berln" (did you mean "Europe/Berlin"?)
hint: list known zones with `belay cron zones`
```

`belay cron zones` lists the names, optionally only those containing a
text (case-insensitively):

```console
$ belay cron zones america/b
America/Bogota
```

Zones whose current rules cannot be written as a POSIX string, such as
Morocco's Ramadan suspensions or the announced dates of Gaza and Hebron,
are deliberately absent; use a fixed offset or a POSIX rule instead.

POSIX rules count offsets the other way round from ISO offsets: they give
the time to *add* to local time to reach UTC, so `EST5` is five hours west
of Greenwich, and `UTC+3` is three hours *behind* UTC while `+03:00` is
three hours ahead:

```console
$ belay cron next "0 12 * * *" --tz "UTC+3" --from 2026-01-01T00:00:00Z -n 1
2026-01-01T12:00:00-03:00
$ belay cron next "0 12 * * *" --tz "+03:00" --from 2026-01-01T00:00:00Z -n 1
2026-01-01T12:00:00+03:00
```

The rule grammar is POSIX.1-2017 section 8.3 with the RFC 8536 extensions
(transition times from -167 to +167 hours):

```text
std offset [dst [offset] [,start[/time],end[/time]]]
```

Transition dates are `Jn` (day 1-365, February 29 never counted), `n`
(day 0-365, February 29 counted) or `Mm.w.d` (weekday `d` of week `w` of
month `m`, `w = 5` meaning the last). A start time is read on the standard
clock and an end time on the daylight clock; both default to 02:00. The
DST offset defaults to one hour ahead of standard time.

Only each zone's current rule is modelled. Historical offsets and past
rule changes are not: `America/Sao_Paulo` is `-03` all year even for 2018
dates, when Brazil still observed daylight saving time. Fire times in
years before a zone's last rule change use today's rule.

## Daylight saving time

Fire times follow the wall clock of the schedule's zone. Two situations
need a rule, because the wall clock and real time stop corresponding one
to one:

**Clocks jump forward.** Readings inside the gap never occur. If any of
them match, the schedule fires once, at the first instant after the gap;
several matches in the gap collapse into that one fire. In New York on
2025-03-09 the clocks go from 02:00 EST straight to 03:00 EDT:

```console
$ belay cron next "30 2 * * *" --tz America/New_York --from 2025-03-07T00:00:00Z -n 4
2025-03-07T02:30:00-05:00
2025-03-08T02:30:00-05:00
2025-03-09T03:00:00-04:00
2025-03-10T02:30:00-04:00
$ belay cron next "*/15 2 * * *" --tz America/New_York --from 2025-03-09T06:00:00Z -n 3
2025-03-09T03:00:00-04:00
2025-03-10T02:00:00-04:00
2025-03-10T02:15:00-04:00
```

**Clocks go back.** The readings of the last hour occur twice. A matching
reading fires only on its first occurrence, unless the expression is
*periodic*: it fires more than once an hour (several seconds or minutes
listed) or every hour (`*` in the hour field). Periodic expressions follow
the wall clock through both passes, as cronie does for its wildcard jobs.
In New York on 2025-11-02 the clocks go from 02:00 EDT back to 01:00 EST:

```console
$ belay cron next "30 1 * * *" --tz America/New_York --from 2025-10-31T00:00:00Z -n 4
2025-10-31T01:30:00-04:00
2025-11-01T01:30:00-04:00
2025-11-02T01:30:00-04:00
2025-11-03T01:30:00-05:00
$ belay cron next "*/30 * * * *" --tz America/New_York --from 2025-11-02T04:45:00Z -n 6
2025-11-02T01:00:00-04:00
2025-11-02T01:30:00-04:00
2025-11-02T01:00:00-05:00
2025-11-02T01:30:00-05:00
2025-11-02T02:00:00-05:00
2025-11-02T02:30:00-05:00
$ belay cron next "0 * * * *" --tz America/New_York --from 2025-11-02T04:30:00Z -n 4
2025-11-02T01:00:00-04:00
2025-11-02T01:00:00-05:00
2025-11-02T02:00:00-05:00
2025-11-02T03:00:00-05:00
```

The same rules hold in every zone. In Berlin, where the clocks change at
02:00 UTC+1 in March and 03:00 UTC+2 in October:

```console
$ belay cron next "30 2 * * *" --tz Europe/Berlin --from 2025-03-29T00:00:00Z -n 3
2025-03-29T02:30:00+01:00
2025-03-30T03:00:00+02:00
2025-03-31T02:30:00+02:00
$ belay cron next "30 2 * * *" --tz Europe/Berlin --from 2025-10-25T00:00:00Z -n 3
2025-10-25T02:30:00+02:00
2025-10-26T02:30:00+02:00
2025-10-27T02:30:00+01:00
```

A daily job therefore runs exactly once on both transition days. Jobs that
must never run twice should not rely on periodic expressions across the
autumn change, or should use UTC.

## Checking expressions from the command line

`belay cron` works offline; it needs no server.

```console
$ belay cron explain "30 9 * * MON-FRI" --tz America/New_York
$ belay cron explain "0 0 L * *" -n 3
$ belay cron next "*/15 * * * *" -n 4
$ belay cron zones
```

| option | meaning | default |
|---|---|---|
| `--tz ZONE` | zone: IANA name, `+05:30` or a POSIX rule | `UTC` |
| `-n`, `--count N` | how many fire times, 1 to 1000 | 5 |
| `--from TIME` | start after this RFC 3339 time instead of now | now |

`explain` prints the expression, its normalised form, an English
description, the zone with its abbreviation and offset, and the next fire
times with their weekday and distance from now. When a clock change falls
among the listed times it adds a note:

```console
$ belay cron explain "0 9 * * *" --tz "CET-1CEST,M3.5.0,M10.5.0/3" --from 2026-03-27T00:00:00Z -n 3
expression  0 9 * * *
meaning     at 09:00 every day
time zone   +01:00 (CET +01:00)
next 3      2026-03-27T09:00:00+01:00  Fri  ...
            2026-03-28T09:00:00+01:00  Sat  ...
            2026-03-29T09:00:00+02:00  Sun  ...
note        clocks go forward at 2026-03-29T01:00:00.000Z (+01:00 to +02:00); fire times follow wall-clock time
```

(The elided column is the distance from the current time, such as `in 3d`.)
`next` prints only the times, one per line, and exits with status 1 when
the expression never fires after the start time. Both take `--json`.
Usage mistakes, including a bad expression or zone, exit with status 2.

## Limitations

- Only each zone's current rule is known; fire times before a zone's last
  rule change use today's rule. The table is only as current as the tzdata
  release it was built from (2026d), and zones without a POSIX footer are
  missing.
- Fire times have one-second resolution, and iteration covers wall-clock
  times from 1970 to the end of 2199, the range of the year field. Searches
  past that return nothing.
- Schedules always use the Vixie dialect with either-field day matching;
  Quartz numbering and both-field matching are available only to Rust code
  calling the crate directly.
- A repeated hour fires a non-periodic expression once, on the first pass.
  There is no option to fire on the second pass instead.
