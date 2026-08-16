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
