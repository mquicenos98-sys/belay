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
