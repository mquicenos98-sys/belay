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
