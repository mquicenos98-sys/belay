# Clustering

Several belay nodes can form a cluster. Each queue and each schedule is
then owned by exactly one live node, and a node asked about a queue or
schedule it does not own answers with a redirect to the owner. That spreads
queues over machines, and moves a dead node's queues to the survivors. It
does not replicate anything. This document says precisely what clustering
does and does not do, how membership works, how to configure a cluster,
and what happens when things fail.

## What clustering does, and what it does not

Clustering decides *where* a queue or schedule is served:

- nodes find each other through seed addresses and track who is alive
  with the SWIM protocol over UDP;
- every node computes the owner of each queue (key `queue:<name>`) and
  each schedule (key `schedule:<id>`) locally, by rendezvous hashing over
  the live members; once the nodes' views agree, so do the owners;
- requests about a queue or schedule owned elsewhere are answered with
  `307 Temporary Redirect` to the owner, and schedules fire only on their
  owner.

It does not decide where data lives:

- **No replication.** A job lives in the store of the node that owned its
  queue when it was enqueued. A node's queues are unavailable while it is
  down, and moving a queue to a new owner moves no data.
- **No consensus.** Views of the membership converge but are not agreed on
  at any instant. For a few gossip rounds after every change, and for the
  whole of a network partition, two nodes may both believe they own a
  queue or a schedule.
- **No shared state.** Every node has its own data directory, its own
  queues, schedules, calendars and workflow runs, and its own event stream.
- **No security on gossip.** Membership datagrams are neither
  authenticated nor encrypted; the cluster name keeps misconfigured
  clusters apart but is not a secret. Run gossip on a trusted network.
- **Nothing is proxied.** Clients follow redirects themselves, with their
  own credentials.

A cluster is useful for spreading independent queues over machines and for
keeping new work flowing when a node dies. It is not a way to make one
queue's jobs survive the loss of a disk.

## Membership

The membership protocol is SWIM (Das, Gupta and Motivala, 2002) with its
suspicion mechanism, the dynamic suspicion timeouts of Lifeguard (Dadgar et
al., 2018), and full-state push-pull exchanges for joining and
anti-entropy.

- **Failure detection.** Every protocol period each node pings one member,
  taken from a list shuffled every round, so every member is probed once
  per round. Without an ack within the probe timeout, it asks a few other
  members to ping the target indirectly. If no ack arrives by the end of the
  period, direct or relayed, the target becomes *suspect*.
- **Suspicion.** A suspect is still a member: it is probed and keeps its
  queues. The suspicion is gossiped. A suspect that hears of it *refutes* it
  by incrementing its incarnation number and gossiping that it is alive,
  which overrides the suspicion everywhere. A suspicion not refuted in time
  turns into *dead*. The timeout starts at its maximum and shrinks towards
  the minimum as other members independently suspect the same node, so a
  real crash is declared quickly while one node with a bad link has a hard
  time getting a healthy member declared dead.
- **Gossip.** Membership updates are piggybacked on every datagram, and
  also sent in dedicated rounds to a few random members, including members
  declared dead recently, so that a node wrongly declared dead finds out.
- **Joining.** A node with seeds asks each of them for its view until one
  answers; the seed merges the newcomer and gossips it.
- **Anti-entropy.** Periodically a node swaps its full view with a random
  member, and also tries to reach a member it believes dead (and its seeds,
  while it has no live peer), which is how the two sides of a healed
  partition find each other again.
- **Leaving.** A node shutting down gracefully gossips that it has *left*;
  departed members are not probed and give up their keys at once, without
  a suspicion period.
- **Restarts.** A node restarting with the same id learns what the cluster
  remembers about it and refutes it with a higher incarnation.

Dead and departed members are remembered for a day (the tombstone time),
then forgotten. Each datagram has a 9-byte header (protocol version,
CRC-32C, and a tag derived from the cluster name); datagrams from another
protocol version or cluster, or that fail the checksum, are dropped. Nodes
send datagrams of at most 1400 bytes.

`belay members` (`GET /v1/cluster/members`) lists the members as the node
asked sees them, with their API address, state (`alive`, `suspect`, `dead`
or `left`) and which one is the *leader*, the live member with the lowest
id. The leader is reported but belay uses it for nothing; every node runs
its own chores.

## Timing

`cluster.profile` chooses one of three timing presets:

| setting | `lan` (default) | `wan` | `local` |
|---|---|---|---|
| probe interval (protocol period) | 1s | 5s | 100ms |
| probe timeout | 500ms | 3s | 50ms |
| indirect probes | 3 | 3 | 3 |
| suspicion multiplier | 4 | 6 | 3 |
| maximum suspicion, as a multiple of the minimum | 6 | 6 | 6 |
| retransmit multiplier | 4 | 4 | 4 |
| gossip interval / fanout | 200ms / 3 | 500ms / 4 | 20ms / 3 |
| gossip to dead members for | 30s | 60s | 5s |
| push-pull interval | 30s | 60s | 2s |
| reconnect interval | 10s | 30s | 500ms |
| join retry interval | 1s | 3s | 100ms |
| leave timeout | 5s | 10s | 1s |
| tombstones kept | 24h | 24h | 24h |

`lan` is for one data centre or cloud region, `wan` for members spread
across regions, `local` for processes on one machine (development and
tests). Several timings scale with the number `n` of live members:

| derived value | formula | 3 nodes: `lan` / `wan` / `local` |
|---|---|---|
| minimum suspicion timeout | multiplier * max(1, log10 n) * probe interval | 4s / 30s / 300ms |
| maximum suspicion timeout | maximum multiple * minimum | 24s / 3m / 1.8s |
| sends per update | retransmit multiplier * ceil(log10(n + 1)) | 4 / 4 / 4 |
| push-pull period | the interval up to 32 members, one more multiple per doubling beyond | 30s / 60s / 2s |
| worst-case time until every member has declared a crashed one dead, without message loss | 2(n - 1) periods + maximum suspicion + gossip rounds for the sends + one period | 29.8s / 3m27s / 2.4s |

The worst case assumes the unluckiest probe order and no confirmations. In
practice the first of the other members to probe the crashed node suspects
it within a few periods, confirmations from the others shrink the
suspicion towards the minimum, and a crash on a `lan` cluster is typically
declared in 5 to 10 seconds. For a 10-node `lan` cluster the worst case is
about 45 seconds.

## Ownership

Every node scores every key with a hash of the pair (node, key), and the
key belongs to the highest-scoring live node (rendezvous or highest random
weight hashing). This needs no state beyond the member list, and moves the
minimum: when a node joins it takes over only the keys it now scores
highest for, about 1/n of them; when one leaves or dies, only its own keys
move, spread evenly over the others. Suspect members count as live and keep
their keys.

A node's share can be changed with a weight in its metadata, a number in
(0, 1000] defaulting to 1; a node of weight 2 owns about twice the keys of
a node of weight 1:

```toml
[cluster]
meta = { zone = "eu-west-1a", weight = "2" }
```

Weighted scores go through a floating-point logarithm, whose last bit may
differ between platforms; nodes on different platforms could then disagree
about a key whose two best scores are within a rounding error, a chance of
roughly 2^-50 per key. Clusters whose weights are all equal compare integer
hashes only.

Ownership is decided per key, not per request. Queues and schedules are
owned; jobs, workflow runs, calendars and events are not:

| resource | served by |
|---|---|
| a queue: enqueue, `GET /v1/jobs?queue=`, lease, every `/v1/queues/:name` call | the queue's owner (redirect) |
| a schedule: every `/v1/schedules/:id` call, and firing | the schedule's owner (redirect) |
| a job by id: get, cancel, retry, promote, delete, heartbeat, complete, fail | the node that stores it; others answer `not_found` and say which node answered |
| workflow runs | the node that accepted the run |
| calendars | every node, from its own store |
| listings of queues, schedules, jobs without `?queue=`, workers | the node asked, showing what it holds |
| the event stream | the node asked, with its own events |

## Redirects

A redirect is a `307` with an absolute `Location` on the owner, for the
same path and query. A 307 keeps the method and body, so a redirected
`POST` is replayed as is. The body says where for people using curl:

```json
{"redirect": {"node": "belay-2", "location": "http://10.0.0.6:7700/v1/queues/emails"}}
```

The Rust client (and so the `belay` command and `belay worker`) follows up
to 5 redirects, forwarding the bearer token, since every node of a cluster
is equally trusted.

Requests naming several queues are split by owner:

- **Enqueue batches** are atomic and nodes share no storage, so a batch
  must be owned by one node: all local is served, all on one other node is
  redirected there, anything else is refused with `400 invalid_argument`,
  whose `details.owners` lists each queue with its node and URL, so the
  client can split the batch.
- **Leases** are served from the queues owned by the node asked. The
  others are skipped and named, comma-separated, in the
  `Belay-Remote-Queues` response header. Only when none of the queues is
  owned there is the lease redirected, to the owner of the first (most
  preferred) queue.

A worker whose queues live on different nodes therefore needs a connection
to each of them. The client does not remember redirects, and a job's
heartbeats and result must go to the node that handed the job out; see the
limitations of [workers.md](workers.md).

## Configuring a three-node cluster

Every node runs `belay server` with its own configuration. For three nodes
on `10.0.0.5`, `10.0.0.6` and `10.0.0.7`, the first one:

```toml
[server]
listen = "0.0.0.0:7700"

[[auth.tokens]]
name = "workers"
token_sha256 = "87cbebfeebc05f7c54ac9336c4b4bbec831227a641951a4bde7edd56020f8590"  # gitleaks:allow
scopes = ["work"]

[storage]
dir = "/var/lib/belay"

[cluster]
enabled = true
node_id = "belay-1"
name = "belay-prod"
gossip_listen = "0.0.0.0:7946"
advertise = "10.0.0.5:7946"
api_addr = "http://10.0.0.5:7700"
seeds = ["10.0.0.6:7946", "10.0.0.7:7946"]
profile = "lan"
join_grace = "10s"
```

The other two differ in `node_id`, `advertise`, `api_addr` and `seeds`.
Everything that must exist on every node (queue configurations,
`[[calendars]]`, `[[schedules]]`) belongs in the part of the file they
share; see [scheduling.md](scheduling.md#clusters).

| key | meaning | default |
|---|---|---|
| `enabled` | join a cluster; without it a single node owns everything | `false` |
| `node_id` | unique in the cluster; letters, digits, `-`, `_`, `.`, `:`, `@`, starting with a letter or digit, at most 128 bytes | the host name (`HOSTNAME` or `COMPUTERNAME`), cleaned |
| `name` | shared by every node of one cluster | `belay` |
| `gossip_listen` | UDP address for membership gossip | `0.0.0.0:7946` |
| `advertise` | gossip address other nodes use | the bound address |
| `api_addr` | this node's API as other nodes redirect clients to it | derived from `server.listen` |
| `seeds` | gossip addresses of nodes to join through | none |
| `profile` | timing preset: `lan`, `wan` or `local` | `lan` |
| `join_grace` | how long a node that has not joined waits before firing schedules and reporting ready | 10s |
| `meta` | free-form metadata gossiped with the node: at most 32 entries, keys of letters, digits, `-`, `_` and `.`, 512 bytes encoded in all; `weight` sets its share of keys | none |

Things the configuration checks:

- With `server.listen` on a wildcard address (`0.0.0.0`, `[::]`), other
  nodes cannot redirect clients to it, so `api_addr` is required; loading
  the configuration fails without it.
- Listening on anything but a loopback address requires `[[auth.tokens]]`
  or `auth.disabled = true`.
- With `gossip_listen` on a wildcard address, `advertise` is required; the
  node refuses to start without it ("set an advertise address other nodes
  can reach").
- Seed host names are resolved once, at start. A seed that does not
  resolve fails the start rather than being skipped, since starting without
  it would make the node a cluster of one that owns everything.

List the other nodes as seeds on every node, so the cluster forms in any
start order. A node without seeds starts as the first node of a new
cluster. Until a node has joined through a seed, `/readyz` answers `503`
("not ready: waiting to join the cluster") and it fires no schedules; after
`join_grace` it does both regardless, so that a node whose seeds are all
down still runs.

A running cluster:

```console
$ belay --url http://10.0.0.5:7700 members
NODE     ADDRESS               STATE  LEADER
belay-1  http://10.0.0.5:7700  alive  yes
belay-2  http://10.0.0.6:7700  alive
belay-3  http://10.0.0.7:7700  alive
```

## Failure scenarios

**A node crashes.** The others notice within seconds (see Timing) and
declare it dead. Its queues and schedules move to the survivors, spread
over them, and requests for those queues are redirected to the new owners,
which start them empty: new jobs are accepted and run there. The crashed
node's own jobs stay in its data directory, including those its workers
were running; nothing can lease them until it comes back. When it returns
with the same id and data directory, it refutes its death, recovers its
store, takes its keys back, and its expired leases send their jobs back to
their queues at its next tick.

**Jobs left behind by a move.** Because ownership follows the membership
and data stays put, a queue can have jobs on two nodes after a move: the
old owner's from before, the new owner's from after. Leases go to the
current owner only, so the other node's jobs wait until their node owns the
queue again (they can still be read, cancelled or deleted by id on that
node). The same happens when a node joins and takes over keys that had
work on other nodes.

**A node shuts down gracefully.** On `belay admin shutdown` the node
gossips that it has left before stopping, and its keys move at once,
without waiting out a suspicion period.

**A node is slow or loses packets.** It becomes suspect, keeps its keys,
and refutes the suspicion as soon as it hears of it. Indirect probes and the
refutation window make false positives rare; the protocol's tests run at
10% packet loss without any.

**The network partitions.** Each side declares the other dead and, within
itself, owns every key. Both sides then accept jobs for the same queue
names, each into its own stores, and fire the schedules they have, so a
schedule defined on every node fires on both sides (the deduplication key
of a fire is per node). When the partition heals, reconnect attempts find
the other side, each side refutes the other's belief that it died, and
ownership converges again; jobs stay where they were enqueued.

**A node cannot reach any seed.** It stays joining, retries its seeds, and
after `join_grace` becomes ready and fires every schedule it has, since
alone it owns them all. If the rest of the cluster is in fact running, that
is a partition of one: see above.

**A node takes over a schedule.** It continues from the present. A fire
the previous owner missed while dying is not caught up, and a fire the
previous owner made just before losing ownership is not known to the new
owner.

## Limitations

- No replication: a node's queues, jobs and schedules are unavailable
  while it is down, and data never moves with ownership.
- No consensus: ownership can briefly overlap after a membership change,
  and for the whole of a partition. Anything that must happen at most once
  needs its own fencing.
- Jobs created by schedules and workflows go into the queues of the node
  that runs them, whichever node owns those queues. Give such jobs queues
  that the same node owns, or run workers against every node, or they wait
  on a node the owner's workers are not redirected to.
- Schedules created through the API exist only on the node that stored
  them; define schedules in the configuration file shared by all nodes.
- Gossip is unauthenticated and unencrypted, over UDP only, with no TCP
  fallback for large state transfers (a full view is split over several
  datagrams, and a lost one is repaired by the next exchange).
- A node that is itself overloaded does not slow its own probing down
  (Lifeguard's local health multiplier is not implemented), so it can
  raise suspicions against healthy members; the dynamic suspicion timeout
  limits the damage.
- Only the three timing presets are available from the configuration file.
- Event sequence numbers, the event stream and listings are per node.
