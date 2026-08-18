# Metrics

belay exports its metrics in the Prometheus text format at `GET /metrics`.
The families come from three places: the engine (`belay_core::metrics`),
the HTTP server's own request metrics, and the process collector of
`belay-metrics`. This document lists every family with its type, labels
and meaning, and ends with queries and alert rules built on them.

## The /metrics endpoint

```console
$ curl -s -H "Authorization: Bearer $BELAY_TOKEN" http://127.0.0.1:7700/metrics
# HELP belay_jobs_enqueued_total Jobs accepted by the queue.
# TYPE belay_jobs_enqueued_total counter
belay_jobs_enqueued_total{queue="default"} 1
...
```

The answer uses the text exposition format 0.0.4 (`Content-Type:
text/plain; version=0.0.4; charset=utf-8`), families sorted by name, with
`# HELP` and `# TYPE` lines. When authentication is enforced the endpoint
needs a token with the `read` scope, like every other `GET`; `/healthz`
and `/readyz` are the only unauthenticated routes. Query parameters are
ignored, so scrapers may add their own. Scrapes of `/metrics`, like the
probes, are logged at debug level only.

A Prometheus scrape configuration:

```yaml
scrape_configs:
  - job_name: belay
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/belay-token
    static_configs:
      - targets: ["10.0.0.5:7700", "10.0.0.6:7700", "10.0.0.7:7700"]
```

In a cluster every node reports only what it holds: its own queues, its
own store, its own events. Scrape every node and aggregate across them.

Two kinds of families behave differently in a scrape:

- **Labelled counters and histograms** (by `queue`, `schedule`, `state`,
  `route`...) appear once they have a series. A queue that has never had a
  job succeed has no `belay_jobs_succeeded_total` series yet.
- **Everything read at scrape time** (queue depths, store figures, bus and
  cluster numbers) is computed from the engine's state when the registry is
  rendered, and is always present.

Lease latencies are sampled by the queue engine and folded into their
histogram every `metrics.refresh` (5 seconds by default), and again at the
start of every scrape.

## Job counters

These counters are fed from the event stream as events are published, so
they agree exactly with it. All have the label `queue`.

| family | type | counts | event |
|---|---|---|---|
| `belay_jobs_enqueued_total` | counter | jobs accepted by the queue (duplicates of a uniqueness key excluded) | `job.enqueued` |
| `belay_jobs_started_total` | counter | attempts started by a lease | `job.started` |
| `belay_jobs_succeeded_total` | counter | jobs completed successfully | `job.succeeded` |
| `belay_jobs_failed_total` | counter | failed attempts that will be retried | `job.failed` |
| `belay_jobs_dead_total` | counter | jobs that failed for good: attempts exhausted or a non-retryable error | `job.dead` |
| `belay_jobs_cancelled_total` | counter | jobs cancelled, before running or after a requested cancellation was acknowledged | `job.cancelled` |
| `belay_jobs_expired_total` | counter | leases that ran out, including attempts that overran their timeout | `job.expired` |

An expired lease is also a failed attempt, so it shows up in
`belay_jobs_failed_total` or `belay_jobs_dead_total` too. An attempt that
fails is counted in exactly one of `failed` (it will be retried), `dead` or
`cancelled`.

Like all counters these start from zero when the process starts; use
`rate()` and `increase()`, which handle resets.

## Queue and worker gauges

Read from the queue engine at scrape time:

| family | type | labels | meaning |
|---|---|---|---|
| `belay_queue_jobs` | gauge | `queue`, `state` | jobs in each of the seven states: `scheduled`, `ready`, `running`, `retrying`, `succeeded`, `dead`, `cancelled` |
| `belay_queue_paused` | gauge | `queue` | 1 if the queue is paused, else 0 |
| `belay_queue_oldest_ready_seconds` | gauge | `queue` | how long the oldest ready job has been eligible; 0 when nothing is ready |
| `belay_workers` | gauge | | workers in the registry: seen recently, or lost less than `engine.worker_retention` ago (or still holding jobs) |
| `belay_lease_waiting` | gauge | | lease requests currently long-polling |

`belay_queue_oldest_ready_seconds` is the best single signal that workers
are falling behind: it grows when jobs are ready and nobody takes them,
whatever the cause (too few workers, a paused queue, a concurrency or rate
limit, workers stuck on another queue).

The terminal-state series of `belay_queue_jobs` count retained jobs, which
retention deletes after a while; they are not cumulative. Use the counters
above for rates.

## Lease latency

| family | type | labels | meaning |
|---|---|---|---|
| `belay_lease_wait_seconds` | histogram | `queue` | time jobs waited between becoming eligible and being leased |

"Eligible" is the job's `run_at`: its enqueue time, the end of its delay,
or the end of a backoff delay. The buckets run from 5 milliseconds to about
5.8 hours, each four times the previous one:

```text
0.005 0.02 0.08 0.32 1.28 5.12 20.48 81.92 327.68 1310.72 5242.88 20971.52 +Inf
```

The queue engine keeps up to 4096 samples between refreshes.

## Schedules and workflows

| family | type | labels | meaning |
|---|---|---|---|
| `belay_schedule_fires_total` | counter | `schedule` | jobs enqueued by schedules (`schedule.fired`), manual triggers included |
| `belay_schedule_misfires_total` | counter | `schedule` | times a schedule found fires missed while belay was down or late (`schedule.misfired`) |
| `belay_schedules` | gauge | | schedules stored on this node |
| `belay_workflows_finished_total` | counter | `state` | workflow runs finished, by outcome: `succeeded`, `failed`, `cancelled` |
| `belay_workflows_running` | gauge | | workflow runs in progress |

A misfire is counted once per schedule and tick, however many fire times
were missed; the event carries the count. In a cluster `belay_schedules`
counts every schedule a node stores, including those another node owns and
fires.

## Storage

Read from the store's statistics at scrape time:

| family | type | meaning |
|---|---|---|
| `belay_store_keys` | gauge | keys in the store |
| `belay_store_live_bytes` | gauge | bytes of keys and values; the dataset held in memory |
| `belay_store_wal_bytes` | gauge | bytes of write-ahead log on disk |
| `belay_store_wal_segments` | gauge | write-ahead log segment files |
| `belay_store_snapshot_bytes` | gauge | size of the newest snapshot |
| `belay_store_last_seq` | gauge | sequence number of the last write |
| `belay_store_checkpoints_total` | counter | checkpoints written since the store was opened |
| `belay_store_wal_syncs_total` | counter | log syncs since the store was opened |

With `storage.in_memory` the log, snapshot, checkpoint and sync figures are
0. `belay_store_wal_bytes` climbs between checkpoints and drops after
each; it staying above `storage.checkpoint_bytes` means checkpoints are
failing. `rate(belay_store_wal_syncs_total[5m])` against the write rate
shows how well group commit is sharing syncs. More figures, including what
recovery did at startup, are in `belay admin store` (see
[storage.md](storage.md)).

## Events, cluster and process

| family | type | labels | meaning |
|---|---|---|---|
| `belay_events_published_total` | counter | | events published on the bus |
| `belay_event_lags_total` | counter | | times a subscriber fell behind the replay buffer and missed events |
| `belay_event_subscribers` | gauge | | event stream subscribers |
| `belay_events_buffered` | gauge | | events held for replay (at most `events.buffer`) |
| `belay_cluster_members` | gauge | `state` | cluster members by state (`alive`, `suspect`, `dead`, `left`) as this node sees them; a node without clustering reports itself as one `alive` member |
| `belay_uptime_seconds` | gauge | | time since the engine started |
| `belay_info` | gauge | `version`, `node` | always 1; carries the version and node id |

With `metrics.process = true` (the default) the process collector adds:

| family | type | available |
|---|---|---|
| `process_start_time_seconds` | gauge | everywhere; the time the collector was created, milliseconds after the process started |
| `process_uptime_seconds` | gauge | everywhere |
| `process_resident_memory_bytes` | gauge | Linux (`/proc/self/status`) |
| `process_virtual_memory_bytes` | gauge | Linux |
| `process_threads` | gauge | Linux |
| `process_open_fds` | gauge | Linux; may read one high, counting the directory handle used to list them |
| `process_max_fds` | gauge | Linux; absent when unlimited |

Where procfs is missing or unreadable those families are simply absent.
