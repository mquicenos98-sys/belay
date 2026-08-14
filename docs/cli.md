# The belay command

`belay` is one binary with three jobs: it runs the server (`server`,
`dev`) and workers (`worker`), it manages a running server over the HTTP
API (jobs, queues, schedules, calendars, workflows, events), and it
checks cron expressions, configuration files and data directories
offline.

```text
belay server [--config PATH]
belay dev [--listen ADDR] [--data DIR | --in-memory] [--workers N] [--queues LIST] [--no-worker]
belay enqueue <QUEUE> <KIND> [--payload JSON|@FILE|-] [--priority N] [--delay D] [--run-at T] ...
belay job      get | list | cancel | retry | promote | delete | wait
belay queue    list | info | pause | resume | purge | config | delete
belay schedule add | list | get | pause | resume | rm | trigger | preview
belay calendar list | get | put | rm
belay workflow submit | get | list | cancel
belay worker [--queues LIST] [--concurrency N] [--shell] [--allow PROGRAM]...
belay workers | members | info | version
belay events tail [--types PATTERNS] [--queue Q] [--from SEQ] [-n N]
belay cron     explain | next | zones
belay config   check | print | default
belay admin    checkpoint | store | verify | shutdown | store-inspect <DIR>
```

`belay help <command>` (or `<command> --help`) prints the flags of any
command, with examples. A mistyped command or flag gets a "did you mean"
hint.

## Global options

These go before or after the command and apply to every command that
talks to a server.

| option | environment | default | meaning |
|---|---|---|---|
| `--url <URL>` | `BELAY_URL` | `http://127.0.0.1:7700` | Server address. `http://` only. |
| `--token <TOKEN>` | `BELAY_TOKEN` | none | Bearer token, sent as `Authorization: Bearer <TOKEN>`. |
| `--json` | | off | Print the API's JSON (pretty) instead of tables; one line per event for `events tail`. |
| `-q`, `--quiet` | | off | Print only identifiers (or a job's result), no confirmations. |
| `--no-color` | `NO_COLOR` | color on terminals | Never color output. Color is also off when stdout is not a terminal or `TERM=dumb`. |
| `-v`, `--verbose` | | off | Log more: debug logs on stderr, and the server's error details. `-vv` for trace logs. |
| `-h`, `--help` | | | Help for the command. |
| `-V`, `--version` | | | Print `belay 0.6.0`. |

A flag given on the command line wins over its environment variable,
which wins over the default. `BELAY_CONFIG` names the configuration file
for `server` and `config`.

Output goes to stdout and everything else (errors, warnings, notes) to
stderr, so pipelines only see data. Tables show times rounded ("3m
ago"); `--json` gives the exact values.

## Exit status

| status | meaning |
|---|---|
| 0 | success |
| 1 | the command failed: the server refused the request, an input file is invalid, a waited-for job did not succeed, a check found problems |
| 2 | the command line is wrong: unknown command or flag, bad value, missing argument |
| 3 | the named job, queue, schedule, calendar or workflow run does not exist |
| 4 | the server is unreachable, unavailable (shutting down) or shedding load (`429`); retrying later may help |

Errors print as `error: <message>`, often followed by a `hint:` line.
Commands on several ids (`job cancel a b c`) try every id, report each
failure, and exit with the status of the first one.

## belay server

Runs the server from a configuration file until it is shut down.

| flag | meaning |
|---|---|
| `-c`, `--config <PATH>` | Configuration file; `BELAY_CONFIG` when absent; the built-in defaults when neither is given. |

`BELAY_<SECTION>__<KEY>` variables override the file (see
[configuration.md](configuration.md#environment-overrides)). Warnings
about unknown keys are printed, then the server starts and reports where
it listens:

```text
$ belay server --config /etc/belay/belay.toml
belay 0.6.0 serving http://0.0.0.0:7700 (node belay-1, storage /var/lib/belay, config /etc/belay/belay.toml)
```

Logs go to stderr in the configured format; `-v`, `-vv` and `-q` replace
the configured level with `debug`, `trace` or `warn`. `belay admin
shutdown` stops the server gracefully. Ctrl-C ends it at once, which is
safe: the store is crash-consistent, and jobs that were running return
to their queues when their leases expire.

```sh
belay server --config /etc/belay/belay.toml
BELAY_SERVER__LISTEN=0.0.0.0:7700 belay server -c belay.toml
```

## belay dev

Runs a development server: no authentication, a loopback address only,
data in memory unless `--data` names a directory, and a worker in the
same process with the `echo`, `sleep`, `fail` and `shell` handlers.

| flag | default | meaning |
|---|---|---|
| `--listen <ADDR>` | `127.0.0.1:7700` | Address to serve on; must be a loopback address. |
| `--data <DIR>` | | Keep data in this directory (it survives restarts). |
| `--in-memory` | on | Keep data in memory, lost on exit. |
| `--workers <N>` | `4` | Slots of the built-in worker. |
| `--queues <LIST>` | `default` | Queues the built-in worker serves, comma-separated or repeated. |
| `--allow <PROGRAM>` | any | Limit shell jobs to these programs (repeatable). |
| `--no-worker` | | Do not run the built-in worker. |

```text
$ belay dev
belay 0.6.0 dev server
  api      http://127.0.0.1:7700
  storage  memory (lost on exit)
  worker   4 slots on default; kinds echo, fail, shell, sleep
try:  belay enqueue default echo --payload '{"hello": "world"}' --wait
stop: belay admin shutdown (or Ctrl-C)
```

The built-in worker runs shell jobs, so anyone who can reach the
address can run programs as you; that is why `dev` refuses non-loopback
addresses.

## belay enqueue

`belay enqueue <QUEUE> <KIND> [flags]` adds a job. Unless `--delay` or
`--run-at` say otherwise the job is ready at once.

| flag | meaning |
|---|---|
| `-d`, `--payload <JSON>` | Payload: inline JSON, `@FILE` to read a file, or `-` for stdin. |
| `-p`, `--priority <N>` | `-1000` to `1000`; higher runs first. Negative values are fine: `--priority -5`. |
| `--max-attempts <N>` | Attempts before the job is dead (default: the queue's). |
| `--retry <POLICY>` | Retry backoff such as `exp:1s*2..10m` (default: the queue's). |
| `--timeout <DURATION>` | Execution budget per attempt (default: the queue's). |
| `-t`, `--tag <TAG>` | Tag the job (repeatable). |
| `--tenant <TENANT>` | Tenant, for fair sharing within the queue. |
| `-m`, `--meta <KEY=VALUE>` | Annotation (repeatable). |
| `--delay <DURATION>` | Run no earlier than this from now. |
| `--run-at <TIME>` | Run no earlier than this RFC 3339 time. |
| `--unique <KEY[:SCOPE]>` | Deduplicate on `KEY`; `SCOPE` is `until_started`, `until_finished` or `for:DURATION`, split off at the last colon, so keys may contain colons. |
| `--on-conflict <POLICY>` | When the key is taken: `reject`, `return_existing` or `replace`. |
| `-w`, `--wait` | Wait for the job to finish and print its result. |
| `--wait-timeout <DURATION>` | Stop waiting after this long (needs `--wait`). |

Without `--wait` it prints what happened (`enqueued 01M3... on emails
(ready)`, or which job already holds the uniqueness key); `-q` prints
just the id, and `--json` the API's result. With `--wait` it polls the
job (at most every half second) until it finishes, prints the outcome
and the result, and exits with status 1 if the job did not succeed or
`--wait-timeout` passed first.

```sh
belay enqueue emails send_welcome --payload '{"user": 42}'
belay enqueue reports build --delay 10m --unique daily-report:for:1h
belay enqueue default echo -d @payload.json --wait
```

```text
$ belay enqueue default echo --payload '{"hello": "world"}' --wait
job 01M3QM66798R5KVRD3G1M0YJ6F succeeded after 1 attempt in 0ms
{
  "hello": "world"
}
```

## belay job

| command | does |
|---|---|
| `job get <ID>` (alias `show`) | Shows a job: state, attempts, errors, result. |
| `job list [flags]` (alias `ls`) | Lists jobs, newest first. |
| `job cancel <ID>...` | Cancels jobs; running ones stop at their next heartbeat. |
| `job retry <ID>...` | Sends dead or cancelled jobs back to ready. |
| `job promote <ID>...` | Makes scheduled or retrying jobs ready now. |
| `job delete <ID>...` (alias `rm`) | Deletes finished jobs. |
| `job wait <ID> [--timeout DURATION]` | Waits for a job to finish and prints its result; status 1 if it did not succeed or the timeout passed. |

`job list` flags:

| flag | meaning |
|---|---|
| `--queue <QUEUE>` | Only this queue. |
| `-s`, `--state <STATE>` | Only these states, comma-separated or repeated. |
| `--kind <KIND>`, `--tag <TAG>`, `--tenant <TENANT>` | Only jobs of this kind, with this tag, of this tenant. |
| `--created-after <TIME>`, `--created-before <TIME>` | Only jobs created in this range (RFC 3339, exclusive). |
| `--order <ORDER>` | `desc` (default, newest first) or `asc`. |
| `-n`, `--limit <N>` | Jobs per page (server maximum 1000). |
| `--cursor <CURSOR>` | Continue after a previous page. |
| `--all` | Follow every page to the end. |

```text
$ belay job list -n 3
ID                          QUEUE    KIND  STATE      TRIES  AGE       INFO
01M3QM6697VYXNS3XKE8FNP587  default  echo  succeeded      1  just now  took 1ms
01M3QM66798R5KVRD3G1M0YJ6F  default  echo  succeeded      1  just now  took 0ms
01M3QM5J3NPPMDRD3FB3HZDMX9  default  echo  succeeded      1  20s ago   took 0ms
showing 3 jobs; continue with --cursor 05tg387f8b47bdd8vgt6yp73yv9tkdr, or use --all
```

`-q` prints one id per line, which combines with the commands taking
several ids:

```sh
belay job list --state dead,retrying --queue emails
belay job list -q --state dead | xargs belay job retry
```

## belay queue

| command | does |
|---|---|
| `queue list` (alias `ls`) | Lists queues with their job counts, oldest ready job and completions per second. |
| `queue info <NAME>` (alias `get`) | Shows a queue's counts, throughput and configuration. |
| `queue pause <NAME>` | Stops handing out jobs from the queue; enqueueing continues. |
| `queue resume <NAME>` | Hands out jobs from a paused queue again. |
| `queue purge <NAME> [--state STATE] --yes` | Removes the queue's jobs, or only those in one state; running jobs are never removed. |
| `queue config <NAME> [--set KEY=VALUE]... [--file PATH]` | Shows or changes a queue's configuration. |
| `queue delete <NAME> --yes` (alias `rm`) | Deletes a queue, its configuration and its jobs. |

`purge` and `delete` refuse to run without `-y`/`--yes` (status 2).

`queue config` without options prints the configuration as `KEY = VALUE`
lines. `--set` takes the same keys, repeated to change several; `none`
removes an optional setting. `--file` replaces the whole configuration
from JSON or TOML (the fields of `[queues.<name>]`).

```sh
belay queue config emails --set max_concurrency=8 --set retention.dead=30d
belay queue config emails --set rate_limit.per_second=5 --set rate_limit.burst=10
belay queue config emails --set rate_limit=none
belay queue purge emails --state dead --yes
```

```text
$ belay queue list
QUEUE    STATE   READY  SCHED  RUNNING  RETRY  DEAD  OLDEST  DONE/S
default  active      0      0        0      0     0       -     0.1
emails   active      0      0        0      0     0       -     0.0
reports  active      1      0        0      0     0     26s     0.0
```

## belay schedule

| command | does |
|---|---|
| `schedule add [ID] [flags]` | Creates a schedule from flags or a file. |
| `schedule list` (alias `ls`) | Lists schedules with their next fire time. |
| `schedule get <ID>` (alias `show`) | Shows a schedule. |
| `schedule pause <ID>`, `schedule resume <ID>` | Stops or restarts firing. |
| `schedule rm <ID>` (alias `delete`) | Deletes a schedule; its jobs stay. |
| `schedule trigger <ID>` | Fires a schedule now, outside its expression. |
| `schedule preview <ID> [-n N]` | Shows the next fire times (default 10). |

`schedule add` takes the schedule inline (`ID`, `--cron`, `--queue` and
`--kind`, plus policy and job flags) or a whole spec with `--file` (JSON
or TOML with the fields of the API and of `[[schedules]]`); the two
cannot be mixed.

| flag | meaning |
|---|---|
| `--file <PATH>` | Load the spec from a JSON or TOML file. |
| `--cron <EXPR>` | Cron expression (5 to 7 fields, or `@daily` and friends). |
| `--queue <QUEUE>`, `--kind <KIND>` | Where the fired jobs go and which handler runs them. |
| `--tz <ZONE>` | Time zone of the expression (default `UTC`). |
| `--misfire <POLICY>` | After downtime: `skip`, `fire_once` or `fire_all[:N]`. |
| `--overlap <POLICY>` | While the last job runs: `allow`, `skip` or `cancel_previous`. |
| `--jitter <DURATION>` | Random delay of up to this much per fire. |
| `--calendar <NAME>` | Only fire when this calendar allows. |
| `--description <TEXT>` | Free text. |
| `--starts-at <TIME>`, `--ends-at <TIME>` | Do not fire before, or at or after, these times. |
| `--paused` | Create it paused. |
| `--replace` | Overwrite an existing schedule with this id instead of failing. |
| `-d`, `-p`, `--max-attempts`, `--retry`, `--timeout`, `-t`, `--tenant`, `-m` | The job template, as for `enqueue`. |

```sh
belay schedule add nightly --cron '0 2 * * *' --tz America/Bogota --queue reports --kind build
belay schedule add --file schedules/nightly.toml --replace
```

```text
$ belay schedule list
ID              CRON        TZ              STATE   NEXT    LAST     FIRES  JOB
hourly          0 * * * *   UTC             active  in 15h  -            0  sync/sync_accounts
nightly-report  30 2 * * *  America/Bogota  active  in 9h   26s ago      1  reports/build
```

## belay calendar

| command | does |
|---|---|
| `calendar list` (alias `ls`) | Lists calendars. |
| `calendar get <NAME>` (alias `show`) | Shows a calendar. |
| `calendar put [NAME] [flags]` | Creates or replaces a calendar from flags or a file. |
| `calendar rm <NAME>` (alias `delete`) | Deletes a calendar no schedule uses. |

`calendar put` flags:

| flag | meaning |
|---|---|
| `--file <PATH>` | Load the spec from a JSON or TOML file. |
| `--tz <ZONE>` | Time zone of the rules (default `UTC`). |
| `--window <[DAYS] HH:MM-HH:MM>` | An allowed time range, optionally on some days (repeatable). A range whose end is not after its start runs past midnight. |
| `--exclude-date <YYYY-MM-DD>` | A date on which nothing fires (repeatable). |
| `--exclude-weekdays <DAYS>` | Weekdays on which nothing fires, such as `sat,sun`. |
| `--description <TEXT>` | Free text. |

```sh
belay calendar put business-hours --tz Europe/Berlin --window 'mon-fri 09:00-17:00' --exclude-date 2025-12-25
belay calendar put --file examples/calendar-business-hours.json
```

## belay workflow

| command | does |
|---|---|
| `workflow submit <FILE> [--wait] [--wait-timeout DURATION]` | Starts a run from a JSON or TOML spec (`-` reads stdin). With `--wait`, follows it until it finishes; status 1 if it did not succeed. |
| `workflow get <ID>` (alias `show`) | Shows a run and its steps. |
| `workflow list [-n N] [--all]` (alias `ls`) | Lists runs, newest first. |
| `workflow cancel <ID>` | Cancels a run and its unfinished steps. |

```text
$ belay workflow submit examples/workflow-etl.json
started workflow 01M3QMR4RJHKW2Q078RW5RG7YD (nightly-etl, 7 steps)
$ belay workflow list
ID                          NAME         STATE    STEPS  CREATED   TOOK
01M3QMR4RJHKW2Q078RW5RG7YD  nightly-etl  running  0/7    just now  -
```

The spec format is described in [http-api.md](http-api.md#workflows) and
[workflows.md](workflows.md).

## belay worker

Runs a worker that leases jobs from the server over HTTP and runs them
with the built-in handlers: `echo` (returns its payload), `sleep` and
`fail`, plus `shell` with `--shell` or `--allow`.

| flag | default | meaning |
|---|---|---|
| `--queues <LIST>` | `default` | Queues to serve, comma-separated or repeated. The server's `engine.queue_order` decides how it takes from several: in turn by default, or in the order listed with `in_order`. |
| `-c`, `--concurrency <N>` | `4` | Jobs run at once. |
| `--id <ID>` | `host:pid` | Worker id, as shown by `belay workers`. |
| `--shell` | | Also run shell jobs, any program. |
| `--allow <PROGRAM>` | | Run shell jobs, only for this program (repeatable). |
| `--lease-for <DURATION>` | `30s` | Lease length; heartbeats renew it every third of it. |
| `--poll-wait <DURATION>` | `10s` | Long-poll wait when idle. |
| `--burst` | | Exit once the queues stay empty for one poll wait. |
| `--max-jobs <N>` | | Exit after this many jobs finished. |

A shell job's payload names the program and its arguments; no shell is
involved unless the program is one:

```sh
belay worker --queues media --allow convert
belay enqueue media shell --payload '{"cmd": ["convert", "in.png", "-resize", "50%", "out.png"], "timeout": "2m"}'
```

The job succeeds with `{"exit_code": 0, "stdout": "...", "stderr": "..."}`
(the last 64 KiB of each stream) when the program exits with 0. Exit
status 75 (`EX_TEMPFAIL`) is a retryable failure, any other status a
permanent one. `--shell` without `--allow` warns: anyone who can enqueue
can then run any program on the worker's machine. The payload fields
(`cmd`, `env`, `cwd`, `stdin`, `timeout`, `retry_on_exit`) are described
in [workers.md](workers.md).

Without `--burst` or `--max-jobs` the worker runs until the process is
stopped; jobs it was running then come back after their lease expires.
When it exits on its own it prints what it did.

```sh
belay worker --queues emails,reports --concurrency 8
belay worker --queues media --allow convert --allow ffmpeg
belay worker --queues default --burst --poll-wait 1s
```

## belay workers, members, info, version

`belay workers` lists the workers the server has heard from recently,
`belay members` the cluster's members as the server sees them (a single
node lists itself), `belay info` the server's version, node, storage,
uptime and readiness, and `belay version` the version of the command
itself.

```text
$ belay workers
WORKER      QUEUES          ACTIVE  LEASED  SUCCEEDED  FAILED  LAST SEEN  ADDRESS
dev-worker  default,emails       0       5          5       0  just now
w1          manual               0       1          0       1  55s ago

$ belay info
server   http://127.0.0.1:7700
version  0.6.0
node     belay-1
storage  memory
started  2026-09-29T22:21:09.012Z (1m ago)
uptime   1m18s
cluster  1 live node
ready    yes
```

## belay events tail

Prints events as they happen. It starts with the oldest event the server
still holds (or `--from`), follows new ones, and reconnects and resumes
after interruptions without gaps or repeats.

| flag | meaning |
|---|---|
| `--types <PATTERNS>` | Only these types, such as `job.*,schedule.fired` (repeatable). |
| `--queue <QUEUE>` | Only events about this queue. |
| `--from <SEQ>` | Start at this sequence number. |
| `-n`, `--limit <N>` | Exit after this many events. |

```text
$ belay events tail --types job.succeeded -n 2
1877706506502147  2026-09-29T22:21:13.933Z  job.succeeded      job=01M3QM3YTBDKTE6E25JG340DV3 queue=emails attempt=1 runtime=0s
1877706506502173  2026-09-29T22:22:06.453Z  job.succeeded      job=01M3QM5J3M942A9EWY0CGZVF4Q queue=default attempt=1 runtime=0s
```

With `--json`, each event is one line of JSON, as the API sends it:

```sh
belay events tail --types job.dead,job.failed --queue emails
belay events tail --json | jq .type
```

## belay cron

Offline tools for cron expressions; no server needed.

| command | does |
|---|---|
| `cron explain <EXPR> [--tz ZONE] [-n N] [--from TIME]` | Describes the expression in English and lists its next fire times (default 5) with their weekday and distance, noting DST changes in between. |
| `cron next <EXPR> [--tz ZONE] [-n N] [--from TIME]` | Prints the next fire times, one per line, in the zone's offset. |
| `cron zones [FILTER]` | Lists the time zone names belay knows, or those containing `FILTER`. |

`--tz` takes an IANA name, a fixed offset (`+05:30`) or a POSIX rule
(default `UTC`); `--from` starts after an RFC 3339 time instead of now.
Expressions have 5 fields (minute hour day month weekday), 6 (with
leading seconds) or 7 (with trailing year), and support names (`JAN`,
`MON`), steps (`*/15`), `L`, `W`, `#` and nicknames such as `@daily`; see
[cron.md](cron.md).

```text
$ belay cron explain "30 9 * * MON-FRI" --tz America/New_York -n 3 --from 2026-03-06T00:00:00Z
expression  30 9 * * MON-FRI
meaning     at 09:30 on Monday through Friday
time zone   America/New_York (EST -05:00)
next 3      2026-03-06T09:30:00-05:00  Fri  207d ago
            2026-03-09T09:30:00-04:00  Mon  204d ago
            2026-03-10T09:30:00-04:00  Tue  203d ago
note        clocks go forward at 2026-03-08T07:00:00.000Z (-05:00 to -04:00); fire times follow wall-clock time

$ belay cron next "*/15 * * * *" -n 2 --from 2026-01-01T00:00:00Z
2026-01-01T00:15:00Z
2026-01-01T00:30:00Z
```

## belay config

Offline tools for configuration files, loading them exactly as `belay
server` does, `BELAY_...` overrides included. Without a path they use
`BELAY_CONFIG`, else the built-in defaults.

| command | does |
|---|---|
| `config check [PATH] [--strict]` | Validates the file. Errors point at the line (status 1); unknown keys are warnings with suggestions, and `--strict` makes them fail too. |
| `config print [PATH]` | Prints the effective configuration as TOML, defaults included, tokens as `token_sha256`. |
| `config default` | Prints a commented example configuration. |

```text
$ belay config check examples/belay.toml
examples/belay.toml: ok
listen     0.0.0.0:7700
storage    /var/lib/belay (sync always)
auth       4 tokens
queues     emails, reports, webhooks
schedules  nightly-sales-report, crm-sync
calendars  business-hours
cluster    off
```

See [configuration.md](configuration.md) for every setting.

## belay admin

| command | does |
|---|---|
| `admin checkpoint` | Writes a storage snapshot now and drops the log it covers. |
| `admin store` | Shows the server's storage statistics. |
| `admin verify` | Checks that the server's job indexes agree with its store; status 1 if they do not. Holds the queue lock for a full scan. |
| `admin shutdown` | Stops the server gracefully: drains requests, then stops the engine. |
| `admin store-inspect <DIR> [--records]` | Lists and verifies the files of a data directory, offline. |

`store-inspect` reads the directory without locking or changing it, so
it also works on a store that is open elsewhere (whose last log segment
may then show a torn tail, which is just a write in progress). It checks
every snapshot and log record checksum and the continuity of sequence
numbers, and fails (status 1) when opening the store would lose data.
`--records` also lists every log record.

```text
$ belay admin store-inspect /var/lib/belay
FILE                          SEQS   ENTRIES     SIZE  NOTE
wal-00000000000000000001.log  1..=4        4  1.6 KiB
snapshot seq 0, recoverable through seq 4
no problems found

$ belay admin shutdown
server at http://127.0.0.1:7700 is shutting down (requests get up to 30s to finish)
```

[operations.md](operations.md) shows these commands in use.
