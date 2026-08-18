# Running belay in production

This guide covers building, installing and running a belay server: the
files it keeps, how to back them up and upgrade, what to watch, how much
it can hold, and what to do when something goes wrong. Configuration is
described key by key in [configuration.md](configuration.md), the storage
engine in [storage.md](storage.md), clusters in [cluster.md](cluster.md)
and the metrics in [metrics.md](metrics.md).

## Building and installing

belay has no dependencies outside the Rust standard library, so a build
needs a Rust toolchain (1.80 or newer) and nothing else: no registry
access, no C compiler, no system libraries.

```sh
cargo build --release -p belay-cli
./target/release/belay version        # belay 0.6.0
```

The result is one self-contained binary, `target/release/belay`
(`belay.exe` on Windows), which is the server, the worker, the admin
tool and the client. Copy it to the machines that need it:

```sh
sudo install -m 0755 target/release/belay /usr/local/bin/belay
```

belay builds and runs on Linux, macOS and Windows. A few operational
details are better on Linux: a lock left by a crashed process is
recognised as stale, and the `process_*` metrics include memory, threads
and file descriptors.

## Directory layout

A typical installation:

| path | contents |
|---|---|
| `/usr/local/bin/belay` | the binary |
| `/etc/belay/belay.toml` | the configuration ([`examples/belay.toml`](../examples/belay.toml)) |
| `/etc/belay/belay.env` | `BELAY_URL` and `BELAY_TOKEN` for the admin commands the service runs |
| `/var/lib/belay` | the data directory, `storage.dir` |

The data directory holds nothing but the store:

```text
LOCK                              exists while a server has the store open; holds its process id
wal-00000000000000000001.log      write-ahead log segments, named after their first sequence number
snap-00000000000000000742.db      the latest snapshot: the state after batch 742
snap-...db.tmp                    a snapshot being written (deleted on open)
snap-...db.corrupt                a snapshot that failed validation, set aside
```

Logs go to stderr, as logfmt or JSON lines (`log.format`); under systemd
they end up in the journal. Nothing else is written anywhere.

## Running under systemd

[`examples/belay.service`](../examples/belay.service) is a unit for a
single server. The parts that are specific to belay:

- `ExecStartPre=belay config check ...` refuses to start on a broken
  configuration and logs why, before the old data directory is touched.
- `ExecStart=belay server --config /etc/belay/belay.toml` runs the
  server in the foreground; `Type=simple` fits.
- The standard library cannot catch signals, so `SIGTERM` ends belay at
  once. The unit stops it gracefully instead: `ExecStop` runs `belay
  admin shutdown` (with an admin token from `EnvironmentFile`) and waits
  for the process to exit, and `TimeoutStopSec` is longer than
  `server.shutdown_grace`. If the drain does not finish in time, systemd
  kills the process, which is still safe.
- `LimitNOFILE` covers `server.max_connections`: every connection is a
  socket and a thread.
- `StateDirectory=belay` creates `/var/lib/belay` owned by the service
  user, and `ReadWritePaths` keeps everything else read-only.

```sh
sudo systemctl enable --now belay
journalctl -u belay -f
```

Workers are ordinary long-running processes too. A unit for `belay
worker` (or your own worker binary) needs no `ExecStop`: when a worker is
killed, the jobs it was running return to their queues once their leases
expire.

## Choosing a sync policy

`storage.sync` decides when an acknowledged write is on stable storage:

| policy | a write returns after | lost on power loss or kernel crash |
|---|---|---|
| `always` (default) | the log is synced | nothing acknowledged |
| `interval:10ms` | the write reaches the operating system | at most the last 10 ms of writes |
| `never` | the write reaches the operating system | writes since the last segment roll, checkpoint or clean shutdown |

A crash of the belay process alone (the machine keeps running) loses
nothing under any policy, and recovery always restores the state after
some prefix of the acknowledged writes: never a hole in the middle.

- Keep **`always`** unless measurements say otherwise. An enqueue,
  completion or failure that the API acknowledged is then durable. The
  queue engine writes under its lock, one change at a time, so every
  enqueue, lease, heartbeat and completion waits for a sync of its own,
  whatever the number of clients. On a disk with slow syncs (network
  block storage, some cloud volumes) that is where the time goes.
- **`interval:<d>`** trades a bounded window of recent writes for sync
  latency off the request path. A job acknowledged in that window can be
  lost, or run again (its completion was lost). If your producers retry
  enqueues with a uniqueness key and your handlers are idempotent,
  which they must be anyway, that is often acceptable.
- **`never`** is for data you can lose: caches, test environments, a
  second copy of work tracked elsewhere.

Batch enqueues (`{"jobs": [...]}`) and multi-job leases (`max_jobs`)
cost one store write each, whatever the policy, which is the cheapest
way to more throughput. Writes from different parts of the engine (the
queue, the scheduler, workflows) that arrive together do share one sync.

## Checkpoints and disk usage

Every change is appended to the log. A checkpoint writes the whole state
to a new snapshot and deletes the log it covers; it runs when the log on
disk reaches `storage.checkpoint_bytes` (128 MiB), at least every
`storage.checkpoint_interval` (1 h) while anything changes, and on
`belay admin checkpoint`. Disk usage therefore stays around one snapshot
(the size of the live data) plus up to `checkpoint_bytes` of log plus one
segment.

Recovery time grows with the log to replay, which is what the interval
bounds for quiet stores. A checkpoint copies the in-memory table, so it
briefly needs as much memory again as the data (see
[Capacity](#capacity)).

## Backups

The store is crash-consistent: any copy that a crash could have left
behind can be opened. Three ways to take a backup, safest first:

1. **Stop the server** (`belay admin shutdown`), copy the data
   directory, start it again.
2. **Snapshot the file system** (LVM, ZFS, a cloud volume snapshot)
   while it runs. That captures what a crash would, which recovery
   handles.
3. **Checkpoint, then copy** while it runs:

   ```sh
   belay admin checkpoint                        # log is now one short segment
   rsync -a --exclude LOCK /var/lib/belay/ /backup/belay-$(date +%F)/
   belay admin store-inspect /backup/belay-$(date +%F)
   ```

   After the checkpoint the directory is one snapshot and a short log,
   so the copy is quick and small. A write in progress at the end of the
   log may be copied half-written; recovery truncates it, which loses
   only that write. `store-inspect` verifies every checksum and the
   continuity of sequence numbers without opening the store, and fails
   (status 1) if the copy would lose data, for example because an
   automatic checkpoint deleted a segment in the middle of the copy; in
   that case copy again.

Leave the `LOCK` file out of copies: on a restored directory it would
make the next start refuse the store (see
[Troubleshooting](#troubleshooting)).

To restore, stop the server, replace the data directory with the copy,
and start it. Jobs that were running when the backup was taken come back
as running with their old leases, which expire at the first tick, so
they run again: another reason for idempotent handlers.

Events are not part of the store and are not backed up; the replay
buffer starts empty after every restart.

## Upgrades

1. Read the [changelog](../CHANGELOG.md) for the versions in between;
   behaviour changes are called out there (0.6.0, for example, lets
   `enqueue` tokens cancel jobs).
2. Take a backup.
3. Check the new binary against your configuration:
   `belay-new config check --strict /etc/belay/belay.toml`.
4. Stop the server gracefully, replace the binary, start it.

Jobs and other records are stored as JSON documents that are decoded but
never re-validated, so a limit tightened in a new release cannot make
existing jobs unreadable, and fields a newer version adds are ignored by
readers that do not know them. The HTTP API stays at `/v1`, and clients
ignore response fields they do not know, so workers and producers can be
upgraded before or after the server.

In a cluster, upgrade one node at a time and wait for its `/readyz`
before moving on. While a node is down its queues are unavailable (there
is no replication); requests for them are redirected to the node that
takes over ownership, which does not have their jobs.

## Monitoring

Three endpoints:

- `GET /healthz` (no token): `200 ok` while the process answers. Use it
  for liveness: restart on failure.
- `GET /readyz` (no token): `503` while shutting down and, in a cluster,
  until the node has joined. Use it for load balancers.
- `GET /metrics` (`read` scope): Prometheus text format.

A Prometheus scrape job with a `read` token:

```yaml
scrape_configs:
  - job_name: belay
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/belay-token
    static_configs:
      - targets: ["10.0.0.11:7700", "10.0.0.12:7700", "10.0.0.13:7700"]
```

What to alert on (the families are described in [metrics.md](metrics.md)):

| signal | metric |
|---|---|
| workers falling behind | `belay_queue_oldest_ready_seconds` above what the queue tolerates |
| jobs failing for good | `rate(belay_jobs_dead_total[5m])`, `belay_queue_jobs{state="dead"}` |
| workers dying or stalling | `rate(belay_jobs_expired_total[5m])` (leases that ran out), `belay_workers` |
| slow pickup | `belay_lease_wait_seconds` percentiles per queue |
| server errors | `belay_http_requests_total{status=~"5.."}` |
| checkpoints not keeping up | `belay_store_wal_bytes` staying well above `storage.checkpoint_bytes` |
| subscribers too slow | `rate(belay_event_lags_total[5m])` |
| cluster degraded | `belay_cluster_members{state!="alive"}` |
| descriptor exhaustion (Linux) | `process_open_fds / process_max_fds` |
| schedules missing fires | `rate(belay_schedule_misfires_total[1h])` |

For a quick look without Prometheus, `belay queue list`, `belay workers`
and `belay events tail` show the same picture, and `belay info` the
server's clock, uptime and readiness.

Every request is logged under the target `belay_server::access` with its
route, status, duration, token name and request id (`X-Request-Id`),
which clients can set to trace a call through their own logs. Probes and
scrapes are logged at `debug` so they do not flood the log;
`level = "info,belay_server::access=warn"` keeps only failed requests.

## Capacity

**Memory.** The store keeps the whole dataset in memory, and the queue
keeps indexes over it. A job with a small payload takes about 400 bytes
of store; payloads and results add their JSON size (each is limited to
1 MiB). What is held is what is retained: pending jobs, plus finished
jobs until their queue's `retention` runs out (dead jobs are kept 7 days
by default), plus finished workflow runs for `workflows.keep_for`. A
checkpoint briefly doubles the store's share. Size the machine for the
retained jobs, not the rate: 10 million retained jobs of a few hundred
bytes is a few gigabytes; the same number of 100 KiB payloads is not
sensible, so keep large inputs elsewhere and pass references.

**Start-up.** Opening the store replays the log since the last snapshot,
and the queue engine then scans every job to rebuild its indexes, so
start-up time grows with the retained jobs. Shorter retention and a
tighter `checkpoint_interval` both shorten it.

**Connections.** Every connection is a thread, including the ones idle
in a long poll or streaming events. `server.max_connections` must cover
every worker slot that may be polling at once, every event subscriber,
and ordinary traffic; beyond it clients get `503` with `Retry-After: 1`.
The file descriptor limit must cover it too.

**Throughput.** Under `sync = "always"` the queue's commit rate is
bounded by the disk's sync latency, one change per sync; batch enqueues
and multi-job leases, which write many changes at once, help the most. `belay-bench` measures store, queue and
HTTP throughput on your hardware (see
[CONTRIBUTING.md](../CONTRIBUTING.md#benchmarks)).

**Scaling out.** A cluster spreads queues over nodes by hashing their
names, so it adds capacity when there are many queues; one busy queue is
always served by one node. See [cluster.md](cluster.md).

## Troubleshooting

**`store is locked by process 1234 (/var/lib/belay/LOCK)`.** Another
process has the directory open, or one that crashed left its lock. Make
sure no belay process uses the directory (`ps`, `systemctl status`). On
Linux a lock whose process no longer exists is taken over automatically;
elsewhere, or when the recorded process id now belongs to an unrelated
process, delete the `LOCK` file by hand. Never run two servers on one
directory.

**The store refuses to open with a corruption error.** A crash leaves at
most a half-written record at the very end of the log; recovery truncates
it with a warning and starts. Damage anywhere else (a flipped bit, a
file truncated by hand, a bad copy) is reported with the file and offset,
and under the default `storage.recovery = "strict"` the server refuses
to start rather than lose data silently. Then:

1. Look at the damage offline: `belay admin store-inspect
   /var/lib/belay` (add `--records` to list every log record).
2. Restore the latest backup if you have one.
3. Otherwise start once with `storage.recovery = "skip-corrupt"` (or
   `BELAY_STORAGE__RECOVERY=skip-corrupt`): it skips the damaged records,
   logs what it skipped, and writes a fresh checkpoint so the damage is
   not met again. `belay admin store` shows what was skipped under
   `recovery`. Go back to `strict` afterwards.

A snapshot that fails validation is skipped when the log still restores
everything it held, and renamed to `*.corrupt`; otherwise it counts as
corruption like any other damage.

**Jobs stay `running` although no worker has them.** A worker that dies
keeps its leases until they expire: `lease_for` after its last heartbeat
(30 s for belay's workers), never later than the job's `timeout`. The
next queue tick then fails the attempt with `lease expired` and the job
retries. Check `belay job list --state running` and `belay workers`
(last seen). Leases are stored with their expiry, so they also run out
if the server itself was down for longer. To stop waiting, `belay job
cancel <id>`: the job is cancelled when its worker's next heartbeat or
its lease expiry comes.

**Jobs stay `ready`.** Something keeps them from being leased:

- the queue is paused (`belay queue info <name>`, STATE `paused`);
- its `max_concurrency` is reached, or its `rate_limit` is spending
  (`belay queue config <name>`);
- no worker serves the queue: `belay workers` shows each worker's
  queues;
- in a cluster, the queue is owned by another node: a lease skips queues
  owned elsewhere and names them in the `Belay-Remote-Queues` response
  header, so the worker needs a connection to that node.

A job whose kind no handler is registered for is not stuck: the worker
fails it at once, non-retryably, with `no handler registered for kind
"x"` in its errors.

**Schedules do not fire.** `belay schedule get <id>` shows the next fire
time; `belay events tail --types schedule.*` shows fires, misfires and
skips (with the reason, `calendar` or `overlap`). Check that the
schedule is not paused and within its `starts_at`/`ends_at`, that
`scheduler.enabled` is true, and in a cluster that the node owning the
schedule is up and has joined (`/readyz`).

**`/readyz` answers 503 in a cluster.** The node has not joined through
any seed and its `cluster.join_grace` has not passed. Check the seeds'
addresses, that UDP port 7946 (or your `gossip_listen`) is open between
the nodes, that every node uses the same `cluster.name`, and that
`cluster.advertise` is an address the others can reach.

**Clients get `503` or connections are refused under load.** The server
is at `server.max_connections`, or the process at its descriptor limit.
Raise both; see [Capacity](#capacity).

**Times look off.** `belay info` prints the server's clock; compare it
with the clients'. `run_at`, delays and lease deadlines use the server's
clock.

## Graceful shutdown

`belay admin shutdown` (or `POST /v1/admin/shutdown` with an `admin`
token) stops a server cleanly:

1. The server stops accepting connections and `/readyz` turns to `503`.
2. Requests in flight finish; long-polling leases answer with no jobs and
   event streams end, so neither holds up the stop by more than a
   fraction of a second.
3. After at most `server.shutdown_grace` (30 s), the engine shuts down:
   background duties stop, the store is flushed and closed, and the
   process exits with status 0.

Results reported while the server drains are still accepted, since the
engine stops last. A worker whose job outlasts the drain gets errors for
its heartbeats and result once the server is gone; that job runs again
after its lease expires on the next start.

Ctrl-C, `SIGTERM` or `kill -9` end the process at once instead: the
standard library cannot catch signals. That is safe. Nothing acknowledged
is lost (under `sync = "always"`), the next start replays the log, and
jobs that were running return to their queues when their leases expire.
`belay worker` behaves the same way when killed. `belay dev` stops its
built-in worker first when asked to shut down, so its jobs finish or are
handed back.
