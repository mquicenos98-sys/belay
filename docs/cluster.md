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
