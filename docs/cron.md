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
